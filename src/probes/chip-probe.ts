import { mergeAttributes, Node } from '@tiptap/core';

/** Throwaway spike extension; the final editor can replace the visual label. */
export const DateChip = Node.create({
  name: 'dateChip',
  group: 'inline',
  inline: true,
  atom: true,
  selectable: true,
  draggable: true,
  addAttributes() {
    return { chipId: { default: 'chip-probe-1' }, label: { default: 'Sep 18 · 5:00 PM' } };
  },
  parseHTML() {
    return [{ tag: 'span[data-datechip]' }];
  },
  renderHTML({ node, HTMLAttributes }) {
    return ['span', mergeAttributes(HTMLAttributes, {
      'data-datechip': '',
      'data-chip-id': node.attrs.chipId,
      class: 'date-chip',
    }), `📅 ${node.attrs.label}`];
  },
});
