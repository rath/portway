// Strings the page writes after load, in every language the site ships. The
// prose lives in the HTML, one directory per language; this file covers only
// the graph controls and installation copy feedback.
//
// Keys are the page's <html lang>: "en", "ko", "zh-Hans", "ja". Placeholders
// are digits the page formatted itself, never anything a reader supplied, which
// is why the templates may carry markup through innerHTML.

export const STRINGS = {
  en: {
    copied: "Copied",
    copySuccess: "Installation command copied.",
    copyError: "Could not copy. The command is selected; copy it manually.",
    replay: "Replay",
    stop: "Stop",
    seed: 'Turn 1: the agent sent <b>{body}</b> bytes. Portway sent the whole context once, zstd-compressed, as <b class="w">{wire}</b> bytes to seed the dictionary.',
    turn: 'Turn {turn}: the agent sent <b>{body}</b> bytes and <b class="w">{wire}</b> crossed the wire.',
  },
  ko: {
    copied: "복사 완료",
    copySuccess: "설치 명령을 복사했습니다.",
    copyError: "복사하지 못했습니다. 선택된 명령을 직접 복사해 주세요.",
    replay: "재생",
    stop: "정지",
    seed: '1턴: 에이전트가 <b>{body}</b>바이트를 보냈습니다. Portway는 사전의 씨앗을 뿌리려고 컨텍스트 전체를 zstd로 압축해 <b class="w">{wire}</b>바이트로 한 번 보냈습니다.',
    turn: '{turn}턴: 에이전트가 <b>{body}</b>바이트를 보냈고, 회선을 건넌 것은 <b class="w">{wire}</b>바이트입니다.',
  },
  "zh-Hans": {
    copied: "已复制",
    copySuccess: "已复制安装命令。",
    copyError: "无法复制。命令已选中，请手动复制。",
    replay: "重播",
    stop: "停止",
    seed: '第 1 轮：智能体发送了 <b>{body}</b> 字节。Portway 为了播下词典，把整个上下文用 zstd 压缩后一次发出，共 <b class="w">{wire}</b> 字节。',
    turn: '第 {turn} 轮：智能体发送了 <b>{body}</b> 字节，而经过链路的只有 <b class="w">{wire}</b> 字节。',
  },
  ja: {
    copied: "コピー済み",
    copySuccess: "インストールコマンドをコピーしました。",
    copyError: "コピーできませんでした。選択されたコマンドを手動でコピーしてください。",
    replay: "再生",
    stop: "停止",
    seed: '1 ターン目：エージェントは <b>{body}</b> バイトを送信しました。Portway は辞書の種を蒔くために、コンテキスト全体を zstd で圧縮した <b class="w">{wire}</b> バイトを一度だけ送りました。',
    turn: '{turn} ターン目：エージェントは <b>{body}</b> バイトを送信し、回線を通過したのは <b class="w">{wire}</b> バイトでした。',
  },
};

// The page declares its own language, and English is what a page without one
// gets. A tag the table does not carry falls back to its language subtag, so a
// browser reporting "ko-KR" for a Korean page still lands on Korean.
export function strings(lang) {
  const tag = String(lang || "");
  return STRINGS[tag] ?? STRINGS[tag.split("-")[0]] ?? STRINGS.en;
}
