type ChipRun = { chip?: { chipId: string; dueAt: string; timezone: string; repeat: 'none' | 'daily' | 'weekly'; label?: string } };
type Run = { t?: unknown; bold?: boolean; italic?: boolean; underline?: boolean } & ChipRun;
export type ChecklistItem = { id: string; text: string; checked: boolean; runs?: Run[]; [key: string]: unknown };
type TextBlock = { type?: string; runs?: Run[]; items?: ChecklistItem[]; [key: string]: unknown };
type NoteContent = { blocks?: TextBlock[]; plain?: string; [key: string]: unknown };

export type TiptapMark = { type: 'bold' | 'italic' | 'underline'; attrs?: Record<string, unknown> };
export type TiptapNode = { type: string; text?: string; marks?: TiptapMark[]; attrs?: Record<string, unknown>; content?: TiptapNode[] };
export type TiptapDoc = { type: 'doc'; content?: TiptapNode[] };

export function emptyContent(): string {
  return JSON.stringify({ blocks: [{ type: 'text', runs: [] }], plain: '' });
}

function parse(contentJson: string): NoteContent {
  try {
    const value: unknown = JSON.parse(contentJson);
    return value && typeof value === 'object' && !Array.isArray(value) ? value as NoteContent : JSON.parse(emptyContent());
  } catch {
    return JSON.parse(emptyContent());
  }
}

export function buildPlain(contentJson: string): string {
  const content = parse(contentJson);
  return (content.blocks ?? []).map((block) =>
    block.type === 'checklist'
      ? (block.items ?? []).map((item) => item.runs ? plainRuns(item.runs) : (typeof item.text === 'string' ? item.text : '')).join('\n')
      : plainRuns(block.runs ?? []),
  ).join('\n');
}

function plainRuns(runs: Run[]): string { return runs.map((run) => typeof run.t === 'string' ? run.t : run.chip?.label ?? '').join(''); }
function runsToNodes(runs: Run[]): TiptapNode[] {
  return runs.flatMap((run) => {
    if (run.chip?.chipId) return [{ type: 'dateChip', attrs: run.chip } as TiptapNode];
    const text = typeof run.t === 'string' ? run.t : '';
    return text ? [{ type: 'text', text, ...(run.bold || run.italic || run.underline ? { marks: (['bold', 'italic', 'underline'] as const).filter((m) => run[m]).map((type) => ({ type })) } : {}) } as TiptapNode] : [];
  });
}

export function textOf(contentJson: string): string {
  return buildPlain(contentJson);
}

export function setContentText(contentJson: string, text: string): string {
  const content = parse(contentJson);
  if (!content.blocks || content.blocks.length === 0) content.blocks = [{ type: 'text', runs: [] }];
  const index = content.blocks.findIndex((block) => block.type === 'text');
  const block = content.blocks[index >= 0 ? index : 0];
  block.type ??= 'text';
  block.runs = [{ t: text }];
  content.plain = textOf(JSON.stringify(content));
  return JSON.stringify(content);
}

/** Convert the app's structured note content into the small TipTap document we edit today. */
export function contentToTiptap(contentJson: string): TiptapDoc {
  const content = parse(contentJson);
  const blocks = content.blocks ?? [];
  const source = blocks.length ? blocks : [{ type: 'text', runs: [] }];
  const paragraphs: TiptapNode[] = [];
  for (const block of source) {
    if (block.type === 'checklist') {
      const items = (block.items ?? []).map((item) => ({
        type: 'checklistItem',
        attrs: { id: typeof item.id === 'string' ? item.id : makeId(), checked: item.checked === true },
        content: [{ type: 'paragraph', content: runsToNodes(item.runs ?? [{ t: item.text ?? '' }]) }],
      }));
      if (items.length) paragraphs.push({ type: 'checklist', content: items });
      continue;
    }
    if (block.type !== 'text') continue;
    const nodes: TiptapNode[] = [];
    for (const run of block.runs ?? []) {
      if (run.chip?.chipId) { nodes.push({ type: 'dateChip', attrs: run.chip }); continue; }
      const text = typeof run.t === 'string' ? run.t : '';
      const parts = text.split('\n');
      parts.forEach((part, index) => {
        if (part) {
          const marks: TiptapMark[] = [];
          if (run.bold) marks.push({ type: 'bold' });
          if (run.italic) marks.push({ type: 'italic' });
          if (run.underline) marks.push({ type: 'underline' });
          nodes.push({ type: 'text', text: part, ...(marks.length ? { marks } : {}) });
        }
        if (index < parts.length - 1) paragraphs.push({ type: 'paragraph', content: nodes.splice(0) });
      });
    }
    paragraphs.push({ type: 'paragraph', content: nodes });
  }
  return { type: 'doc', content: paragraphs.length ? paragraphs : [{ type: 'paragraph' }] };
}

/** Serialize TipTap JSON back to the persisted blocks/runs shape; `plain` is the search index. */
export function tiptapToContent(doc: TiptapDoc): string {
  const blocks: TextBlock[] = [];
  for (const node of doc.content ?? []) {
    if (node.type === 'checklist') {
      blocks.push({ type: 'checklist', items: (node.content ?? []).filter((item) => item.type === 'checklistItem').map((item) => ({
        id: typeof item.attrs?.id === 'string' ? item.attrs.id : makeId(),
        checked: item.attrs?.checked === true,
        text: inlinePlain(item.content ?? []),
        ...(inlineRuns(item.content ?? []).some((run) => run.chip) ? { runs: inlineRuns(item.content ?? []) } : {}),
      })) });
      continue;
    }
    if (node.type !== 'paragraph') continue;
    blocks.push({ type: 'text', runs: (node.content ?? []).flatMap((child): Run[] => child.type === 'dateChip' && child.attrs?.chipId ? [{ chip: child.attrs as Run['chip'] }] : child.type === 'text' && typeof child.text === 'string' ? (() => {
      const run: Run = { t: child.text };
      for (const mark of child.marks ?? []) {
        if (mark.type === 'bold') run.bold = true;
        if (mark.type === 'italic') run.italic = true;
        if (mark.type === 'underline') run.underline = true;
      }
      return [run];
    })() : []) });
  }
  const normalized = { blocks: blocks.length ? blocks : [{ type: 'text', runs: [] }], plain: '' };
  normalized.plain = buildPlain(JSON.stringify(normalized));
  return JSON.stringify(normalized);
}

function inlinePlain(nodes: TiptapNode[]): string { return nodes.flatMap((p) => p.content ?? []).map((child) => child.type === 'dateChip' ? (child.attrs?.label ?? '') : child.text ?? '').join(''); }
function inlineRuns(nodes: TiptapNode[]): Run[] { return (nodes.flatMap((p) => p.content ?? [])).flatMap((child): Run[] => child.type === 'dateChip' && child.attrs?.chipId ? [{ chip: child.attrs as Run['chip'] }] : child.type === 'text' ? [{ t: child.text }] : []); }

function makeId(): string {
  return globalThis.crypto?.randomUUID?.() ?? `item-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
