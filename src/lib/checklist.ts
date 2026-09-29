import { mergeAttributes, Node } from '@tiptap/core';
import { Fragment } from '@tiptap/pm/model';
import { TextSelection } from '@tiptap/pm/state';

export function newChecklistItem() {
  return {
    type: 'checklistItem',
    attrs: { id: globalThis.crypto?.randomUUID?.() ?? `item-${Date.now()}-${Math.random().toString(16).slice(2)}`, checked: false },
    content: [{ type: 'paragraph' }],
  };
}

export const Checklist = Node.create({
  name: 'checklist',
  group: 'block',
  content: 'checklistItem+',
  parseHTML() { return [{ tag: 'div[data-checklist]' }]; },
  renderHTML({ HTMLAttributes }) { return ['div', mergeAttributes(HTMLAttributes, { 'data-checklist': '' }), 0]; },
});

export const ChecklistItem = Node.create({
  name: 'checklistItem',
  content: 'paragraph',
  defining: true,
  addAttributes() {
    return {
      id: { default: null },
      checked: { default: false },
    };
  },
  parseHTML() { return [{ tag: 'div[data-checklist-item]' }]; },
  addKeyboardShortcuts() {
    return {
      Enter: () => {
        const { state, view } = this.editor;
        const { $from } = state.selection;
        let depth = $from.depth;
        while (depth > 0 && $from.node(depth).type.name !== 'checklistItem') depth -= 1;
        if (depth === 0) return false;

        const item = $from.node(depth);
        const paragraph = item.firstChild;
        if (!paragraph) return false;
        const itemPos = $from.before(depth);
        const offset = $from.parentOffset;
        const left = paragraph.type.create(paragraph.attrs, paragraph.content.cut(0, offset));
        const right = paragraph.type.create(paragraph.attrs, paragraph.content.cut(offset));
        const leftItem = item.type.create(item.attrs, left);
        const rightItem = item.type.create({
          ...item.attrs,
          id: globalThis.crypto?.randomUUID?.() ?? `item-${Date.now()}-${Math.random().toString(16).slice(2)}`,
          checked: false,
        }, right);
        const transaction = state.tr.replaceWith(itemPos, itemPos + item.nodeSize, Fragment.fromArray([leftItem, rightItem]));
        transaction.setSelection(TextSelection.near(transaction.doc.resolve(itemPos + leftItem.nodeSize + 2)));
        view.dispatch(transaction);
        view.focus();
        return true;
      },
    };
  },
  renderHTML({ node, HTMLAttributes }) {
    return ['div', mergeAttributes(HTMLAttributes, {
      'data-checklist-item': '', 'data-item-id': node.attrs.id, 'data-checked': String(node.attrs.checked),
    }), ['input', { type: 'checkbox', checked: node.attrs.checked ? 'checked' : null }], ['div', { class: 'checklist-item-text' }, 0]];
  },
  addNodeView() {
    return ({ node, getPos, editor }) => {
      const dom = document.createElement('div');
      dom.className = 'checklist-item';
      dom.dataset.checked = String(node.attrs.checked === true);
      const checkbox = document.createElement('input');
      checkbox.type = 'checkbox';
      checkbox.checked = node.attrs.checked === true;
      checkbox.setAttribute('aria-label', 'Complete checklist item');
      const contentDOM = document.createElement('div');
      contentDOM.className = 'checklist-item-text';
      dom.append(checkbox, contentDOM);
      checkbox.addEventListener('mousedown', (event) => event.preventDefault());
      checkbox.addEventListener('change', () => {
        if (typeof getPos !== 'function') return;
        const position = getPos();
        if (position === undefined) return;
        editor.view.dispatch(editor.state.tr.setNodeMarkup(position, undefined, {
          ...node.attrs,
          checked: checkbox.checked,
        }).setMeta('checklistToggle', true));
      });
      return {
        dom,
        contentDOM,
        update(updatedNode) {
          if (updatedNode.type !== node.type) return false;
          checkbox.checked = updatedNode.attrs.checked === true;
          dom.dataset.checked = String(updatedNode.attrs.checked === true);
          node = updatedNode;
          return true;
        },
      };
    };
  },
});
