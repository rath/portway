#!/usr/bin/env python3
"""End-to-end request compression benchmark for Portway.

Runs the real `portway` binary as a receiver in front of a local origin, then
as a sender in three configurations, and replays the same append-only chat
conversation through each:

    identity   --coding off   no request compression
    zstd       --dict off     zstd on every eligible body
    dcz        (defaults)     zstd, plus the previous turn as a dictionary

Every byte count comes from the sender's /__portway/stats counters, measured
around each request. The origin checks each restored body's SHA-256 against
what the client sent, so a reported saving is also a verified lossless round
trip. Payloads are generated from a fixed seed: the same seed, level and
Cargo.lock produce the same byte counts on every machine.

Exit status: 0 success, 1 a process or request failed, 2 usage error,
3 a compression invariant did not hold (for example no dictionary hits).
Needs only the Python 3.9+ standard library.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import platform
import random
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Iterator

REPO = Path(__file__).resolve().parent.parent
DEFAULT_BINARIES = [REPO / "target" / "release" / "portway", REPO / "target" / "debug" / "portway"]

# Receiver defaults (crates/portway-core/src/receiver.rs): bodies outside this
# range are never stored, so they can never become dictionaries.
MIN_DICT_BYTES = 32 * 1024
MAX_DICT_BYTES = 32 * 1024 * 1024

# Text model. Prose is built from a Zipf-ranked pool of short phrases, and each
# simulated source file reuses its own small set of identifiers, so bodies
# repeat themselves the way real conversations do. Calibrated so plain zstd at
# level 11 saves about 77%, slightly more than a chat body made of this
# repository's own sources and docs (76.5%): the zstd-only baseline is not
# understated. Uniformly random words would compress far worse.
VOCAB_SIZE = 8192
PHRASES = 4096
ZIPF_S = 1.0
ONSETS = ["b", "c", "d", "f", "g", "h", "j", "k", "l", "m", "n", "p", "r", "s", "t", "v",
          "w", "z", "ch", "sh", "th", "st", "tr", "pl", "br", "cl", "gr", "sp", "fl", "pr"]
VOWELS = ["a", "e", "i", "o", "u", "ai", "ea", "ou", "ie", "oo"]
CODAS = ["", "", "", "n", "t", "s", "r", "l", "d", "m", "ng", "st", "nt", "ck", "sh"]

CHAT_PATH = "/v1/chat/completions"
MODEL = "bench-model"
READY_TIMEOUT = 20.0  # longer than the sender's 15 s capability probe
STOP_TIMEOUT = 5.0
REQUEST_TIMEOUT = 60.0


class BenchError(Exception):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code


# --------------------------------------------------------------------------
# Payload


class Corpus:
    """Deterministic prose and code built from one seeded generator."""

    def __init__(self, rng: random.Random) -> None:
        self.rng = rng
        words: set[str] = set()
        while len(words) < VOCAB_SIZE:
            syllables = rng.randint(1, 3)
            words.add("".join(
                rng.choice(ONSETS) + rng.choice(VOWELS) + rng.choice(CODAS)
                for _ in range(syllables)
            ))
        # The most frequent words are the shortest, as in natural language.
        self.vocab = sorted(words, key=lambda w: (len(w), w))
        self.vocab_cum = zipf_cum(VOCAB_SIZE)
        self.phrases = [" ".join(self.words(rng.randint(2, 5))) for _ in range(PHRASES)]
        self.phrase_cum = zipf_cum(PHRASES)

    def words(self, count: int) -> list[str]:
        return self.rng.choices(self.vocab, cum_weights=self.vocab_cum, k=count)

    def prose(self, nbytes: int) -> str:
        parts: list[str] = []
        size = 0
        while size < nbytes:
            phrases = self.rng.choices(self.phrases, cum_weights=self.phrase_cum, k=self.rng.randint(2, 5))
            sentence = " ".join(phrases)
            sentence = sentence[0].upper() + sentence[1:]
            sentence += self.rng.choice([". ", ". ", ". ", ", ", "? ", "! ", ".\n\n"])
            parts.append(sentence)
            size += len(sentence)
        return "".join(parts)[:nbytes]

    def code(self, nbytes: int) -> str:
        """A source file: a few dozen identifiers, used over and over."""
        names = ["_".join(self.words(2)) for _ in range(self.rng.randint(20, 40))]
        types = [name.title().replace("_", "") for name in names[:8]]

        def name() -> str:
            return self.rng.choice(names)

        lines: list[str] = []
        size = 0
        depth = 0
        while size < nbytes:
            kind = self.rng.random()
            if depth == 0 or kind < 0.10:
                line = (f"fn {name()}({name()}: &{self.rng.choice(types)}) "
                        f"-> Result<{self.rng.choice(types)}, Error> {{")
                depth = 1
            elif kind < 0.45:
                line = f"let {name()} = {name()}.{name()}()?;"
            elif kind < 0.60:
                line = f"if {name()}.is_empty() {{ return Err(Error::{self.rng.choice(types)}); }}"
            elif kind < 0.85:
                line = "// " + self.prose(self.rng.randint(30, 90)).strip()
            else:
                line = "}"
                depth = 0
            text = "    " * depth + line + "\n"
            lines.append(text)
            size += len(text)
        return "".join(lines)[:nbytes]


def zipf_cum(n: int) -> list[float]:
    """Cumulative 1/rank^s weights for random.choices."""
    total = 0.0
    cumulative: list[float] = []
    for rank in range(1, n + 1):
        total += 1.0 / rank**ZIPF_S
        cumulative.append(total)
    return cumulative


class Conversation:
    """An OpenAI-style chat request that only ever grows at the end."""

    def __init__(self, corpus: Corpus) -> None:
        self.corpus = corpus
        self.rng = corpus.rng
        self.calls = 0
        self.messages: list[dict] = [{"role": "system", "content": corpus.prose(4096)}]

    def body(self) -> bytes:
        # "messages" is the last key, so each body is the previous one with
        # its closing "]}" replaced by new messages: append-only on the wire.
        request = {"model": MODEL, "stream": False, "temperature": 0.2, "messages": self.messages}
        return json.dumps(request, separators=(",", ":")).encode()

    def tool_round(self, nbytes: int) -> None:
        self.calls += 1
        call_id = f"call_{self.calls:04d}"
        path = f"src/{'_'.join(self.corpus.words(2))}.rs"
        self.messages.append({
            "role": "assistant",
            "content": None,
            "tool_calls": [{
                "id": call_id,
                "type": "function",
                "function": {"name": "read_file", "arguments": json.dumps({"path": path})},
            }],
        })
        self.messages.append({"role": "tool", "tool_call_id": call_id, "content": self.corpus.code(nbytes)})

    def chat_round(self, nbytes: int) -> None:
        self.messages.append({"role": "assistant", "content": self.corpus.prose(nbytes // 2)})
        self.messages.append({"role": "user", "content": self.corpus.prose(nbytes - nbytes // 2)})

    def fill_to(self, nbytes: int) -> None:
        self.messages.append({"role": "user", "content": self.corpus.prose(1024)})
        while len(self.body()) < nbytes:
            if self.rng.random() < 0.4:
                self.tool_round(self.rng.randint(2000, 8000))
            else:
                self.chat_round(self.rng.randint(1500, 6000))

    def advance(self, turn: int, nbytes: int) -> None:
        if turn % 3 == 0:
            self.tool_round(nbytes)
        else:
            self.chat_round(nbytes)


def iter_turns(seed: int, context_bytes: int, turn_bytes: int, turns: int) -> Iterator[bytes]:
    """One body per turn, generated lazily: the same seed yields the same
    bytes for every configuration, and only one body is held at a time."""
    conversation = Conversation(Corpus(random.Random(seed)))
    conversation.fill_to(context_bytes)
    yield conversation.body()
    for turn in range(2, turns + 1):
        conversation.advance(turn, turn_bytes)
        yield conversation.body()


# --------------------------------------------------------------------------
# Origin: the application behind the receiver


class OriginHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: "Origin"

    def do_POST(self) -> None:
        length = self.headers.get("Content-Length")
        if length is None:
            self.reply(411, {"error": "the receiver always sends Content-Length"})
            return
        remaining = int(length)
        chunks: list[bytes] = []
        while remaining > 0:
            chunk = self.rfile.read(remaining)
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        body = b"".join(chunks)
        self.server.digests.append(hashlib.sha256(body).hexdigest())
        tokens = len(body) // 4
        self.reply(200, {
            "id": "chatcmpl-bench",
            "object": "chat.completion",
            "model": MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": tokens, "completion_tokens": 1, "total_tokens": tokens + 1},
        })

    def do_GET(self) -> None:
        # No capability advertisement: only the receiver may enable compression.
        self.reply(404, {"error": "not found"})

    def reply(self, status: int, payload: dict) -> None:
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, format: str, *args: object) -> None:
        pass


class Origin(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), OriginHandler)
        self.digests: list[str] = []
        self.thread = threading.Thread(target=self.serve_forever, name="origin", daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server_address[1]}"

    def stop(self) -> None:
        self.shutdown()
        self.server_close()


# --------------------------------------------------------------------------
# Portway processes


def free_port() -> int:
    # Another process could take the port before portway binds it. That race
    # surfaces as a readiness failure with the log tail, which is enough here.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(port: int, method: str, path: str, body: bytes | None = None,
            headers: dict[str, str] | None = None,
            timeout: float = REQUEST_TIMEOUT) -> tuple[int, http.client.HTTPMessage, bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        connection.request(method, path, body=body, headers=headers or {})
        response = connection.getresponse()
        return response.status, response.headers, response.read()
    finally:
        connection.close()


def tail(path: Path, lines: int = 20) -> str:
    try:
        text = path.read_text(errors="replace").splitlines()
    except OSError:
        return "(no log)"
    return "\n".join("    " + line for line in text[-lines:]) or "    (empty log)"


@dataclass
class Portway:
    name: str
    port: int
    log: Path
    process: subprocess.Popen
    log_file: object = field(repr=False)

    @classmethod
    def start(cls, binary: Path, name: str, mode: str, upstream: str, workdir: Path,
              extra: list[str]) -> "Portway":
        port = free_port()
        log = workdir / f"{name}.log"
        argv = [str(binary)]
        if mode == "receive":
            argv.append("receive")
        argv += ["--upstream", upstream, "--host", "127.0.0.1", "--port", str(port),
                 "--data-dir", str(workdir / name)] + extra
        log_file = open(log, "wb")
        # cwd is the temporary directory, so a portway.toml in the caller's
        # directory cannot change the configuration being measured.
        process = subprocess.Popen(argv, cwd=workdir, env={**os.environ, "NO_COLOR": "1"},
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                   stderr=log_file)
        return cls(name, port, log, process, log_file)

    def wait_ready(self) -> None:
        deadline = time.monotonic() + READY_TIMEOUT
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise BenchError(1, f"{self.name} exited with status {self.process.returncode}:\n{tail(self.log)}")
            try:
                status, _, data = request(self.port, "GET", "/__portway/health", timeout=1.0)
                if status == 200 and json.loads(data).get("status") == "ok":
                    return
            except (OSError, ValueError, http.client.HTTPException):
                pass
            time.sleep(0.05)
        raise BenchError(1, f"{self.name} was not ready after {READY_TIMEOUT:.0f} s:\n{tail(self.log)}")

    def stats(self) -> dict:
        status, _, data = request(self.port, "GET", "/__portway/stats", timeout=5.0)
        if status != 200:
            raise BenchError(1, f"{self.name}: /__portway/stats returned {status}")
        return json.loads(data)

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(STOP_TIMEOUT)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log_file.close()


# --------------------------------------------------------------------------
# Measurement


@dataclass
class Config:
    name: str
    label: str
    flags: list[str]
    coding: str | None  # negotiated coding expected in stats
    dictionary: bool  # expected "dict" in stats


CONFIGS = [
    Config("identity", "identity (`--coding off`)", ["--coding", "off"], None, False),
    Config("zstd", "zstd only (`--dict off`)", ["--dict", "off"], "zstd", False),
    Config("dcz", "zstd + previous-turn dictionary (default)", [], "zstd", True),
]


@dataclass
class Turn:
    body: int
    wire: int
    coding: str


def run_config(binary: Path, config: Config, receiver: Portway, origin: Origin,
               bodies: Iterator[bytes], workdir: Path, level: int | None) -> tuple[list[Turn], dict]:
    extra = list(config.flags)
    if level is not None:
        extra += ["--level", str(level)]
    sender = Portway.start(binary, f"sender-{config.name}", "forward",
                           f"http://127.0.0.1:{receiver.port}", workdir, extra)
    try:
        sender.wait_ready()
        route = sender.stats()["models"]["upstream"]
        if (route["coding"], route["dict"]) != (config.coding, config.dictionary):
            raise BenchError(3, (
                f"{config.name}: negotiated coding={route['coding']} dict={route['dict']}, "
                f"expected coding={config.coding} dict={config.dictionary}"
            ))
        # A separate credential per configuration keeps dictionary scopes apart.
        headers = {"Content-Type": "application/json", "Authorization": f"Bearer bench-{config.name}"}
        turns: list[Turn] = []
        for index, body in enumerate(bodies, start=1):
            before = sender.stats()["models"]["upstream"]
            status, _, reply = request(sender.port, "POST", CHAT_PATH, body, headers)
            if status != 200:
                raise BenchError(1, f"{config.name} turn {index}: status {status}: {reply[:200]!r}")
            if not origin.digests or origin.digests[-1] != hashlib.sha256(body).hexdigest():
                raise BenchError(1, f"{config.name} turn {index}: the origin did not receive the body that was sent")
            after = sender.stats()["models"]["upstream"]
            delta = {key: after[key] - before[key]
                     for key in ("requests", "body_bytes", "wire_bytes", "encoded_requests", "dict_hits")}
            if delta["requests"] != 1 or delta["body_bytes"] != len(body):
                raise BenchError(1, f"{config.name} turn {index}: counters moved unexpectedly: {delta}")
            if delta["dict_hits"] == 1:
                coding = "dcz"
            elif delta["encoded_requests"] == 1:
                coding = after["coding"] or "identity"
            else:
                coding = "identity"
            turns.append(Turn(delta["body_bytes"], delta["wire_bytes"], coding))
        return turns, sender.stats()["models"]["upstream"]
    finally:
        sender.stop()


def check_invariants(config: Config, stats: dict, turns: int) -> list[str]:
    problems = []
    for key in ("dict_misses", "retried_identity", "upstream_errors", "client_aborts"):
        if stats[key]:
            problems.append(f"{config.name}: {key} = {stats[key]}")
    encoded = 0 if config.coding is None else turns
    if stats["encoded_requests"] != encoded:
        problems.append(f"{config.name}: encoded_requests = {stats['encoded_requests']}, expected {encoded}")
    hits = turns - 1 if config.dictionary else 0
    if stats["dict_hits"] != hits:
        problems.append(f"{config.name}: dict_hits = {stats['dict_hits']}, expected {hits}")
    return problems


# --------------------------------------------------------------------------
# Report


def percent(saved: int, total: int) -> str:
    return f"{100.0 * saved / total:.1f}%" if total else "-"


def report(binary: Path, version: str, advertised: str, args: argparse.Namespace,
           results: dict[str, list[Turn]], receiver_stats: dict) -> None:
    level = args.level if args.level is not None else "default"
    print(f"portway   {binary} ({version})")
    print(f"python    {platform.python_version()}")
    print(f"payload   seed {args.seed}, {args.context_kb} KiB context, {args.turn_kb} KiB added per turn, "
          f"{args.turns} turns, level {level}")
    print(f"receiver  advertises {advertised}")
    print()

    header = f"{'turn':>4}  {'body bytes':>11}  {'identity':>11}  {'zstd only':>11}  {'zstd + dict':>11}  coding"
    print(header)
    print("-" * len(header))
    bodies = [turn.body for turn in results["identity"]]
    for index, body in enumerate(bodies):
        identity = results["identity"][index]
        zstd = results["zstd"][index]
        dcz = results["dcz"][index]
        note = "  (seed: sent in full, stored as the dictionary)" if index == 0 else ""
        print(f"{index + 1:>4}  {body:>11,}  {identity.wire:>11,}  {zstd.wire:>11,}  "
              f"{dcz.wire:>11,}  {dcz.coding}{note}")
    print()

    later = slice(1, None)
    body_later = sum(bodies[later])
    body_session = sum(bodies)
    count = len(bodies) - 1
    print(f"Turns 2-{len(bodies)} are after the seed; the session columns include turn 1.")
    print()
    print("| Method | Wire bytes per turn | Saved per turn | Session wire bytes | Saved over session |")
    print("| --- | ---: | ---: | ---: | ---: |")
    for config in CONFIGS:
        turns = results[config.name]
        wire_later = sum(t.wire for t in turns[later])
        wire_session = sum(t.wire for t in turns)
        print(f"| {config.label} | {round(wire_later / count):,} | "
              f"{percent(body_later - wire_later, body_later)} | {wire_session:,} | "
              f"{percent(body_session - wire_session, body_session)} |")
    print()
    print(f"Mean body per turn after the seed: {round(body_later / count):,} bytes. "
          f"Session body total: {body_session:,} bytes.")
    print("receiver  " + ", ".join(
        f"{key} {receiver_stats[key]:,}"
        for key in ("decoded_requests", "dict_stored", "dict_hits", "dict_misses",
                    "dictionary_entries", "dictionary_bytes")
    ))


# --------------------------------------------------------------------------
# Main


def resolve_binary(option: str | None) -> Path:
    if option:
        path = Path(option)
        if os.sep not in option:
            found = shutil.which(option)
            if found:
                path = Path(found)
        path = path.resolve()
        if not path.is_file():
            raise BenchError(1, f"--portway {option}: no such file")
        return path
    for candidate in DEFAULT_BINARIES:
        if candidate.is_file():
            return candidate
    found = shutil.which("portway")
    if found:
        return Path(found).resolve()
    raise BenchError(1, "no portway binary: run `cargo build --release --locked` or pass --portway PATH")


def version_of(binary: Path) -> str:
    try:
        completed = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=10)
    except OSError as err:
        raise BenchError(1, f"{binary}: {err}") from err
    if completed.returncode != 0:
        raise BenchError(1, f"{binary} --version exited with status {completed.returncode}")
    return completed.stdout.strip()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Measure Portway request compression on a growing chat conversation.",
    )
    parser.add_argument("--portway", metavar="PATH",
                        help="portway binary (default: target/release, then target/debug, then PATH)")
    parser.add_argument("--turns", type=int, default=8, help="requests per configuration, at least 2 (default 8)")
    parser.add_argument("--context-kb", type=int, default=256,
                        help="size of the first request in KiB, at least 32 (default 256)")
    parser.add_argument("--turn-kb", type=int, default=4, help="KiB appended per turn, at least 1 (default 4)")
    parser.add_argument("--level", type=int, help="zstd level 1-19 for the sender (default: portway's)")
    parser.add_argument("--seed", type=int, default=1, help="payload seed (default 1)")
    parser.add_argument("--keep", action="store_true", help="keep the temporary directory with logs and databases")
    args = parser.parse_args(argv)
    if args.turns < 2:
        parser.error("--turns must be at least 2: turn 1 only seeds the dictionary")
    if args.context_kb * 1024 < MIN_DICT_BYTES:
        parser.error(f"--context-kb must be at least {MIN_DICT_BYTES // 1024}: "
                     "the receiver stores no smaller body as a dictionary")
    if args.turn_kb < 1:
        parser.error("--turn-kb must be at least 1")
    if args.level is not None and not 1 <= args.level <= 19:
        parser.error("--level must be between 1 and 19")
    final = (args.context_kb + args.turns * args.turn_kb) * 1024
    if final >= MAX_DICT_BYTES:
        parser.error(f"the last body would reach {final // 1024} KiB; the receiver stores at most "
                     f"{MAX_DICT_BYTES // 1024 // 1024} MiB as a dictionary")
    return args


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        binary = resolve_binary(args.portway)
        version = version_of(binary)
    except BenchError as err:
        print(f"bench: {err}", file=sys.stderr)
        return err.code
    workdir = Path(tempfile.mkdtemp(prefix="portway-bench-"))
    keep = args.keep
    origin: Origin | None = None
    receiver: Portway | None = None
    try:
        origin = Origin()
        receiver = Portway.start(binary, "receiver", "receive", origin.url, workdir, [])
        receiver.wait_ready()
        status, headers, _ = request(receiver.port, "GET", "/__portway/capabilities", timeout=5.0)
        encodings = [v.strip() for v in (headers.get("x-request-encodings") or "").split(",") if v.strip()]
        dictionaries = [v.strip() for v in (headers.get("x-request-dictionary") or "").split(",") if v.strip()]
        if status != 200 or "zstd" not in encodings or "dcz" not in dictionaries:
            raise BenchError(3, f"the receiver did not advertise zstd and dcz (status {status}, "
                                f"encodings {encodings}, dictionaries {dictionaries})")
        advertised = ", ".join(encodings + dictionaries)

        results: dict[str, list[Turn]] = {}
        problems: list[str] = []
        for config in CONFIGS:
            bodies = iter_turns(args.seed, args.context_kb * 1024, args.turn_kb * 1024, args.turns)
            turns, stats = run_config(binary, config, receiver, origin, bodies, workdir, args.level)
            results[config.name] = turns
            problems += check_invariants(config, stats, args.turns)
        sizes = {name: [turn.body for turn in turns] for name, turns in results.items()}
        if len({tuple(size) for size in sizes.values()}) != 1:
            raise BenchError(1, "configurations were sent different bodies; the payload is not deterministic")
        report(binary, version, advertised, args, results, receiver.stats()["receiver"])
        if problems:
            raise BenchError(3, "compression invariants failed:\n  " + "\n  ".join(problems))
        return 0
    except BenchError as err:
        keep = True
        print(f"bench: {err}", file=sys.stderr)
        return err.code
    except KeyboardInterrupt:
        return 130
    finally:
        if receiver is not None:
            receiver.stop()
        if origin is not None:
            origin.stop()
        if keep:
            print(f"bench: logs and databases kept in {workdir}", file=sys.stderr)
        else:
            shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
