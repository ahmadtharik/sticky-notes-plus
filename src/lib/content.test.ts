import { describe, expect, it } from 'vitest';
import { contentToTiptap, emptyContent, setContentText, textOf, tiptapToContent } from './content';

describe('note content', () => {
  it('emptyContent has the spec shape', () => {
    const c = JSON.parse(emptyContent());
    expect(c.blocks).toEqual([{ type: 'text', runs: [] }]);
    expect(c.plain).toBe('');
  });
  it('setContentText stores text and rebuilds plain', () => {
    const out = JSON.parse(setContentText(emptyContent(), 'buy milk\nand eggs'));
    expect(out.blocks[0].runs).toEqual([{ t: 'buy milk\nand eggs' }]);
    expect(out.plain).toBe('buy milk\nand eggs');
  });
  it('textOf round-trips', () => expect(textOf(setContentText(emptyContent(), 'hi'))).toBe('hi'));
  it('converts formatted TipTap runs while rebuilding the plain index', () => {
    const out = JSON.parse(tiptapToContent({ type: 'doc', content: [{ type: 'paragraph', content: [
      { type: 'text', text: 'bold', marks: [{ type: 'bold' }] },
      { type: 'text', text: ' italic', marks: [{ type: 'italic' }] },
      { type: 'text', text: ' under', marks: [{ type: 'underline' }] },
    ] }, { type: 'paragraph', content: [{ type: 'text', text: 'next' }] }] }));
    expect(out.blocks[0].runs).toEqual([{ t: 'bold', bold: true }, { t: ' italic', italic: true }, { t: ' under', underline: true }]);
    expect(out.plain).toBe('bold italic under\nnext');
  });
  it('loads persisted runs into a TipTap document', () => {
    const doc = contentToTiptap(JSON.stringify({ blocks: [{ type: 'text', runs: [{ t: 'hello', bold: true }] }], plain: 'hello' }));
    expect(doc.content?.[0].content?.[0]).toEqual({ type: 'text', text: 'hello', marks: [{ type: 'bold' }] });
  });
  it('round-trips checklist blocks with stable item ids and plain search text', () => {
    const source = JSON.stringify({ blocks: [
      { type: 'text', runs: [{ t: 'Plan' }] },
      { type: 'checklist', items: [
        { id: 'item-a', text: 'draft', checked: false },
        { id: 'item-b', text: 'review', checked: true },
      ] },
    ], plain: 'stale' });
    const doc = contentToTiptap(source);
    const out = JSON.parse(tiptapToContent(doc));
    expect(out.blocks[1]).toEqual({ type: 'checklist', items: [
      { id: 'item-a', text: 'draft', checked: false },
      { id: 'item-b', text: 'review', checked: true },
    ] });
    expect(out.plain).toBe('Plan\ndraft\nreview');
  });
  it('round-trips date chips as structured inline runs and indexes their label', () => {
    const chip = { chipId: 'chip-1', dueAt: '2026-09-18T17:00:00.000Z', timezone: 'America/Toronto', repeat: 'weekly' as const, label: 'Fri, Sep 18, 1:00 PM' };
    const out = JSON.parse(tiptapToContent({ type: 'doc', content: [{ type: 'paragraph', content: [
      { type: 'text', text: 'Pay ' }, { type: 'dateChip', attrs: chip }, { type: 'text', text: ' rent' },
    ] }] }));
    expect(out.blocks[0].runs).toEqual([{ t: 'Pay ' }, { chip }, { t: ' rent' }]);
    expect(out.plain).toBe('Pay Fri, Sep 18, 1:00 PM rent');
    expect(contentToTiptap(JSON.stringify(out)).content?.[0].content?.[1]).toEqual({ type: 'dateChip', attrs: chip });
  });
  it('preserves a chip inside checklist item positional runs', () => {
    const chip = { chipId: 'chip-item', dueAt: '2026-09-18T17:00:00.000Z', timezone: 'UTC', repeat: 'daily' as const, label: 'Sep 18' };
    const doc = contentToTiptap(JSON.stringify({ blocks: [{ type: 'checklist', items: [{ id: 'item-1', checked: false, text: 'Do Sep 18', runs: [{ t: 'Do ' }, { chip }, { t: '!' }] }] }], plain: '' }));
    const out = JSON.parse(tiptapToContent(doc));
    expect(out.blocks[0].items[0].runs).toEqual([{ t: 'Do ' }, { chip }, { t: '!' }]);
    expect(out.plain).toBe('Do Sep 18!');
  });
});
