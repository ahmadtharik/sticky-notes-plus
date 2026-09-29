import { mergeAttributes, Node } from '@tiptap/core';

export type ChipRepeat = 'none' | 'daily' | 'weekly';
export type DateChipAttrs = { chipId: string; dueAt: string; timezone: string; repeat: ChipRepeat; label?: string };

export function makeChipId(): string {
  return globalThis.crypto?.randomUUID?.() ?? `chip-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

export function chipLabel(attrs: Pick<DateChipAttrs, 'dueAt'>): string {
  const date = new Date(attrs.dueAt);
  if (Number.isNaN(date.getTime())) return 'Set date';
  return new Intl.DateTimeFormat(undefined, { weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }).format(date);
}

export const DateChip = Node.create({
  name: 'dateChip', inline: true, group: 'inline', atom: true, selectable: true, draggable: false,
  addAttributes() { return {
    chipId: { default: null }, dueAt: { default: '' }, timezone: { default: 'UTC' }, repeat: { default: 'none' },
    label: { default: '' },
  }; },
  parseHTML() { return [{ tag: 'span[data-datechip]' }]; },
  renderHTML({ node, HTMLAttributes }) {
    const label = node.attrs.label || chipLabel(node.attrs as DateChipAttrs);
    return ['span', mergeAttributes(HTMLAttributes, { 'data-datechip': '', 'data-chip-id': node.attrs.chipId, class: 'date-chip', title: label }), `📅 ${label}`];
  },
});

export function newDateChip(dueAt = new Date(Date.now() + 3600000).toISOString()): { type: 'dateChip'; attrs: DateChipAttrs } {
  const attrs: DateChipAttrs = { chipId: makeChipId(), dueAt, timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC', repeat: 'none' };
  attrs.label = chipLabel(attrs);
  return { type: 'dateChip', attrs };
}
