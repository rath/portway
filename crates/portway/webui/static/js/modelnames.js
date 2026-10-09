// Presentation metadata for this console. Records and selection keys stay raw.
let aliases = new Map();
let revision = 0;

export function setModelAliases(value = {}) {
  const next = new Map(Object.entries(value));
  if (next.size === aliases.size && [...next].every(([id, name]) => aliases.get(id) === name)) return false;
  aliases = next;
  revision++;
  return true;
}

export function aliasRevision() { return revision; }
export function displayModel(id) { return aliases.get(id) ?? id; }

/** Usage notes have an exact model/tier prefix; leave all prose untouched. */
export function displayNotes(table) {
  const names = table.rows.map((row) => {
    const tier = row.tier == null ? "" : ` · ${row.tier}`;
    return [`${row.model}${tier}:`, `${displayModel(row.model)}${tier}:`];
  }).sort((a, b) => b[0].length - a[0].length);
  return table.notes.map((note) => {
    const pair = names.find(([prefix]) => note.startsWith(prefix));
    return pair ? pair[1] + note.slice(pair[0].length) : note;
  });
}
