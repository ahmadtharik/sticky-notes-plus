<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { Editor } from '@tiptap/core';
  import StarterKit from '@tiptap/starter-kit';
  import Underline from '@tiptap/extension-underline';
  import { Checklist, ChecklistItem, newChecklistItem } from '../../lib/checklist';
  import { DateChip, newDateChip, chipLabel, type ChipRepeat } from '../../lib/dateChip';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { createNote, destroyNoteWindow, dragNoteWindow, getNote, openBoard, openNoteWindow, saveNote, saveWindowGeom, softDelete, updateNoteMeta } from '../../lib/api';
  import { contentToTiptap, emptyContent, tiptapToContent } from '../../lib/content';
  import { useDebouncedSave } from '../../lib/useDebouncedSave';

  let { noteId }: { noteId: string } = $props();
  let host = $state<HTMLDivElement>();
  let editor = $state<Editor | undefined>(undefined);
  let noteColor = $state('#FFF9B1');
  let currentContent = $state(emptyContent());
  let status = $state<'idle' | 'saving' | 'error'>('idle');
  let loaded = $state(false);
  let showColors = $state(false);
  let chipPopover = $state<{ open: boolean; x: number; y: number; pos: number; chipId: string; dueAt: string; repeat: ChipRepeat }>({ open: false, x: 0, y: 0, pos: -1, chipId: '', dueAt: '', repeat: 'none' });
  let slashMenu = $state<{ open: boolean; x: number; y: number; selected: number }>({ open: false, x: 0, y: 0, selected: 0 });
  let unlistenClose: (() => void) | undefined;
  let chipAnchor: { left: number; right: number; top: number; bottom: number } | undefined;
  const slashCommands: ReadonlyArray<{ id: string; label: string; hint: string; icon: string; disabled?: boolean }> = [
    { id: 'date', label: 'Date & time', hint: 'Insert a reminder', icon: '📅' },
    { id: 'checklist', label: 'Checklist item', hint: 'Add a checkbox', icon: '☑' },
    { id: 'image', label: 'Insert image', hint: 'Coming soon', icon: '▧', disabled: true },
  ] as const;
  const noteColors = [
    { value: '#FFF9B1', name: 'Butter yellow' }, { value: '#DCE8FF', name: 'Sky blue' },
    { value: '#E4D7FF', name: 'Lavender' }, { value: '#D9F7BE', name: 'Mint green' },
    { value: '#FFD6E7', name: 'Blush pink' }, { value: '#FFE2B8', name: 'Peach' }
  ];
  // A child WebView can begin evaluating before Tauri injects its window
  // metadata. Resolve it after mount instead of crashing during startup.
  let appWindow: ReturnType<typeof getCurrentWindow> | undefined;
  const saver = useDebouncedSave(() => currentContent, async (content) => {
    status = 'saving';
    try { await saveNote(noteId, content); status = 'idle'; }
    catch (error) { status = 'error'; console.error(error); throw error; }
  });

  async function load() {
    try {
      const note = await getNote(noteId);
      if (note) { currentContent = note.content; noteColor = note.color; }
      loaded = true;
      await tick();
      editor = new Editor({ element: host, extensions: [StarterKit, Underline, Checklist, ChecklistItem, DateChip], content: contentToTiptap(currentContent),
        onUpdate: ({ editor: current, transaction }) => { currentContent = tiptapToContent(current.getJSON()); transaction.getMeta('checklistToggle') ? void saver.flush() : saver.schedule(); } });
      host?.addEventListener('click', onEditorClick);
      host?.addEventListener('keydown', onEditorKeydown);
    } catch (error) { status = 'error'; console.error(error); }
  }
  async function flush() { if (editor) currentContent = tiptapToContent(editor.getJSON()); await saver.flush(); }
  async function persistGeometry(floating = true) {
    if (!appWindow) return;
    try {
      const position = await appWindow.outerPosition(); const size = await appWindow.innerSize(); const scale = await appWindow.scaleFactor();
      const p = position.toLogical(scale); const s = size.toLogical(scale);
      await saveWindowGeom(noteId, p.x, p.y, s.width, s.height, floating);
    } catch (error) { console.error('Unable to save note window geometry', error); }
  }
  async function close() {
    await flush();
    await persistGeometry(true);
    editor?.destroy();
    // The close-request listener is useful for native window-close gestures,
    // but must not hold the shell open while the button performs destruction.
    unlistenClose?.();
    unlistenClose = undefined;
    try { await destroyNoteWindow(noteId); }
    catch (error) { status = 'error'; console.error('Unable to close note window', error); }
  }
  async function dragWindow(event: PointerEvent) {
    if (event.button !== 0 || (event.target as HTMLElement)?.closest('button, input, select, textarea, a')) return;
    try { await dragNoteWindow(noteId); } catch (error) { console.error('Unable to drag note window', error); }
  }
  async function newSticky() {
    try { const note = await createNote(); await openNoteWindow(note.id); }
    catch (error) { status = 'error'; console.error(error); }
  }
  async function showBoard() {
    try { await openBoard(); } catch (error) { status = 'error'; console.error(error); }
  }
  function addChecklist() {
    editor?.chain().focus().insertContent({ type: 'checklist', content: [newChecklistItem()] }).run();
  }
  function addDateChip() { editor?.chain().focus().insertContent(newDateChip()).run(); }
  function closeSlashMenu() { slashMenu.open = false; }
  function openSlashMenu() {
    if (!editor) return;
    const { from } = editor.state.selection;
    const parent = editor.state.selection.$from.parent;
    if (!parent.isTextblock || parent.textContent.length > 0) return;
    try {
      const caret = editor.view.coordsAtPos(from);
      slashMenu = { open: true, x: caret.left, y: caret.bottom + 6, selected: 0 };
    } catch { /* The view can be detached while a note is closing. */ }
  }
  function onEditorKeydown(event: Event) {
    const keyEvent = event as KeyboardEvent;
    if (slashMenu.open) {
      if (keyEvent.key === 'ArrowDown' || keyEvent.key === 'ArrowUp') {
        keyEvent.preventDefault();
        const direction = keyEvent.key === 'ArrowDown' ? 1 : -1;
        let next = slashMenu.selected;
        for (let i = 0; i < slashCommands.length; i += 1) {
          next = (next + direction + slashCommands.length) % slashCommands.length;
          if (!slashCommands[next].disabled) break;
        }
        slashMenu.selected = next;
        return;
      }
      if (keyEvent.key === 'Escape') { keyEvent.preventDefault(); closeSlashMenu(); return; }
      if (keyEvent.key === 'Enter') {
        keyEvent.preventDefault(); chooseSlashCommand(slashCommands[slashMenu.selected].id); return;
      }
      // Any ordinary typing dismisses the palette and continues in the editor.
      closeSlashMenu();
    }
    if (keyEvent.key === '/' && !keyEvent.ctrlKey && !keyEvent.metaKey && !keyEvent.altKey) {
      const parent = editor?.state.selection.$from.parent;
      if (parent?.isTextblock && parent.textContent.length === 0) {
        keyEvent.preventDefault();
        openSlashMenu();
      }
    }
  }
  function chooseSlashCommand(id: string) {
    if (id === 'image') return;
    closeSlashMenu();
    if (id === 'checklist') { addChecklist(); return; }
    if (id === 'date') {
      const chip = newDateChip();
      editor?.chain().focus().insertContent(chip).run();
      const chipId = String(chip.attrs.chipId);
      requestAnimationFrame(() => {
        const currentEditor = editor;
        if (!currentEditor) return;
        currentEditor.state.doc.descendants((node, pos) => {
          if (node.type.name === 'dateChip' && node.attrs.chipId === chipId) {
            const rect = currentEditor.view.coordsAtPos(pos);
            openChipPopover(rect, { pos, chipId, dueAt: toDateInput(String(node.attrs.dueAt)), repeat: (node.attrs.repeat || 'none') as ChipRepeat });
          }
        });
      });
    }
  }
  function onEditorClick(event: Event) {
    const target = (event.target as HTMLElement).closest<HTMLElement>('[data-datechip]');
    if (!target || !editor) return;
    let found: { pos: number; attrs: Record<string, unknown> } | undefined;
    editor.state.doc.descendants((node, pos) => { if (node.type.name === 'dateChip' && node.attrs.chipId === target.dataset.chipId) found = { pos, attrs: node.attrs }; });
    if (!found) return;
    // Clicking the active chip toggles its editor, which makes the picker
    // usable without requiring a separate Cancel action.
    if (chipPopover.open && chipPopover.chipId === String(found.attrs.chipId)) {
      chipPopover.open = false;
      return;
    }
    const rect = target.getBoundingClientRect();
    openChipPopover(rect, { pos: found.pos, chipId: String(found.attrs.chipId), dueAt: toDateInput(String(found.attrs.dueAt)), repeat: (found.attrs.repeat || 'none') as ChipRepeat });
  }
  function openChipPopover(rect: { left: number; right: number; top: number; bottom: number }, values: Pick<typeof chipPopover, 'pos' | 'chipId' | 'dueAt' | 'repeat'>) {
    chipAnchor = { left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom };
    chipPopover = { open: true, x: rect.left, y: rect.bottom + 6, ...values };
    requestAnimationFrame(repositionChipPopover);
  }
  function repositionChipPopover() {
    if (!chipPopover.open || !chipAnchor) return;
    const popup = document.querySelector<HTMLElement>('.chip-popover');
    if (!popup) return;
    const margin = 8;
    const gap = 6;
    const width = popup.offsetWidth;
    const height = popup.offsetHeight;
    let x = Math.min(Math.max(margin, chipAnchor.left), Math.max(margin, window.innerWidth - width - margin));
    let y = chipAnchor.bottom + gap;
    if (y + height > window.innerHeight - margin) y = chipAnchor.top - height - gap;
    y = Math.min(Math.max(margin, y), Math.max(margin, window.innerHeight - height - margin));
    if (x !== chipPopover.x || y !== chipPopover.y) chipPopover = { ...chipPopover, x, y };
  }
  function dismissChipOnOutside(event: PointerEvent) {
    const target = event.target as HTMLElement;
    if (!target.closest('.chip-popover, [data-datechip]')) chipPopover.open = false;
  }
  function onDocumentKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && (chipPopover.open || showColors)) {
      chipPopover.open = false;
      showColors = false;
      event.stopPropagation();
    }
  }
  function toDateInput(value: string): string { const date = new Date(value); if (Number.isNaN(date.getTime())) return ''; const pad = (n: number) => String(n).padStart(2, '0'); return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`; }
  function saveChip() { if (!editor || chipPopover.pos < 0 || !chipPopover.dueAt) return; const dueAt = new Date(chipPopover.dueAt).toISOString(); const attrs = { chipId: chipPopover.chipId, dueAt, timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC', repeat: chipPopover.repeat, label: chipLabel({ dueAt }) }; editor.view.dispatch(editor.state.tr.setNodeMarkup(chipPopover.pos, undefined, attrs)); chipPopover.open = false; void saver.flush(); }
  function removeChip() { if (!editor || chipPopover.pos < 0) return; editor.view.dispatch(editor.state.tr.delete(chipPopover.pos, chipPopover.pos + 1)); chipPopover.open = false; void saver.flush(); }
  async function chooseColor(color: string) {
    if (color === noteColor) return;
    const previous = noteColor;
    noteColor = color;
    try { await updateNoteMeta(noteId, { color }); }
    catch (error) { noteColor = previous; status = 'error'; console.error(error); }
  }
  async function deleteNote() {
    try {
      await flush();
      await softDelete(noteId);
      showColors = false;
      await destroyNoteWindow(noteId);
    } catch (error) { status = 'error'; console.error('Unable to delete note', error); }
  }
  function dismissMenu(event: PointerEvent) {
    const target = event.target as HTMLElement;
    if (!target.closest('.options-menu, .menu-button')) showColors = false;
  }
  onMount(() => {
    try { appWindow = getCurrentWindow(); } catch { /* Browser-preview mode. */ }
    void load(); const onBlur = () => { void flush(); }; window.addEventListener('blur', onBlur);
    document.addEventListener('pointerdown', dismissChipOnOutside);
    document.addEventListener('pointerdown', dismissMenu);
    document.addEventListener('keydown', onDocumentKeydown);
    window.addEventListener('resize', repositionChipPopover);
    window.addEventListener('scroll', repositionChipPopover, true);
    if (appWindow) {
      void appWindow.onCloseRequested(async () => { await flush(); await persistGeometry(true); }).then((unlisten) => { unlistenClose = unlisten; }).catch((error) => console.error('Unable to listen for note close', error));
    }
    return () => { unlistenClose?.(); window.removeEventListener('blur', onBlur); document.removeEventListener('pointerdown', dismissChipOnOutside); document.removeEventListener('pointerdown', dismissMenu); document.removeEventListener('keydown', onDocumentKeydown); window.removeEventListener('resize', repositionChipPopover); window.removeEventListener('scroll', repositionChipPopover, true); host?.removeEventListener('click', onEditorClick); host?.removeEventListener('keydown', onEditorKeydown); editor?.destroy(); }; });
</script>

<main style={`background:${noteColor}`}>
  <div class="drag" onpointerdown={dragWindow} aria-label="Drag note window"></div>
  <header onpointerdown={(event) => event.stopPropagation()}>
    <button class:active={showColors} class="menu-button" onclick={() => { showColors = !showColors; }} aria-label="Options" title="Options">•••</button>
    {#if showColors}
      <div class="options-menu" role="menu" aria-label="Note options">
        <div class="colors-dropdown" role="group" aria-label="Note color">
          {#each noteColors as color}
            <button class:selected={noteColor === color.value} class="swatch" style={`background:${color.value}`} aria-label={`Choose ${color.name}`} aria-pressed={noteColor === color.value} title={color.name} onclick={() => void chooseColor(color.value)}></button>
          {/each}
        </div>
        <button class="board-option" role="menuitem" onclick={() => { showColors = false; void showBoard(); }}><span class="menu-icon">☰</span> Notes list</button>
        <button class="delete-option" role="menuitem" onclick={() => void deleteNote()}><span class="menu-icon">&#128465;</span> Delete note</button>
      </div>
    {/if}
    <button onclick={() => void close()} aria-label="Close note" title="Close note">×</button>
  </header>
  {#if loaded}<div bind:this={host} class="editor" aria-label="Note text"></div>{:else}<p class="loading">Loading…</p>{/if}
  {#if slashMenu.open}
    <div class="slash-menu" style={`left:${slashMenu.x}px;top:${slashMenu.y}px`} role="menu" aria-label="Insert content">
      <div class="slash-title">Insert</div>
      {#each slashCommands as command, index}
        <button class:selected={slashMenu.selected === index} class:disabled={command.disabled} role="menuitem" aria-disabled={command.disabled} disabled={command.disabled} onclick={() => chooseSlashCommand(command.id)}>
          <span class="slash-icon">{command.icon}</span><span class="slash-copy"><strong>{command.label}</strong><small>{command.hint}</small></span>
        </button>
      {/each}
      <div class="slash-help">↑↓ navigate · Enter select · Esc close</div>
    </div>
  {/if}
  {#if chipPopover.open}
    <div class="chip-popover" style={`left:${chipPopover.x}px;top:${chipPopover.y}px`} role="dialog" aria-label="Edit date reminder">
      <label>Date and time <input type="datetime-local" bind:value={chipPopover.dueAt} /></label>
      <label>Repeat <select bind:value={chipPopover.repeat}><option value="none">Does not repeat</option><option value="daily">Daily</option><option value="weekly">Weekly</option></select></label>
      <div class="chip-actions"><button onclick={saveChip}>Save</button><button onclick={removeChip}>Remove</button><button onclick={() => chipPopover.open = false}>Cancel</button></div>
    </div>
  {/if}
  <footer class="toolbar" aria-label="Formatting tools">
    <button onclick={() => void newSticky()} aria-label="New sticky" title="New sticky">+</button>
    <button class:active={editor?.isActive('bold')} onclick={() => editor?.chain().focus().toggleBold().run()} aria-label="Bold" title="Bold"><strong>B</strong></button>
    <button class:active={editor?.isActive('italic')} onclick={() => editor?.chain().focus().toggleItalic().run()} aria-label="Italic" title="Italic"><em>I</em></button>
    <button class:active={editor?.isActive('underline')} onclick={() => editor?.chain().focus().toggleUnderline().run()} aria-label="Underline" title="Underline"><u>U</u></button>
    <button class:active={editor?.isActive('strike')} onclick={() => editor?.chain().focus().toggleStrike().run()} aria-label="Strikethrough" title="Strikethrough"><s>ab</s></button>
    <button class:active={editor?.isActive('bulletList')} onclick={() => editor?.chain().focus().toggleBulletList().run()} aria-label="Bullet list" title="Bullet list">☷</button>
    <button disabled aria-label="Insert image" title="Insert image (coming soon)">▧</button>
  </footer>
</main>

<style>
  :global(body) { margin: 0; overflow: hidden; }
  main { box-sizing: border-box; height: 100vh; min-height: 260px; padding: 0 12px 12px; border-radius: 9px; box-shadow: 0 3px 14px #0003; font: 16px system-ui, sans-serif; display: flex; flex-direction: column; overflow: hidden; }
  .drag { height: 16px; flex: 0 0 16px; cursor: grab; }
  header { height: 24px; flex: 0 0 24px; display: flex; justify-content: flex-end; align-items: center; gap: 2px; position: relative; opacity: 0; transition: opacity .12s ease; }
  main:hover header, main:focus-within header, header:has(.options-menu) { opacity: 1; }
  header button { border: 0; border-radius: 3px; background: transparent; min-width: 22px; height: 22px; cursor: pointer; color: #555; }
  header button:hover, header button.active { background: #00000018; }
  header button.swatch { min-width: 16px; width: 16px; height: 16px; padding: 0; border: 1px solid #0003; border-radius: 0; }
  header button.swatch:hover, header button.swatch.selected { outline: 2px solid #555; outline-offset: 1px; }
  header button:last-child { font-size: 20px; line-height: 1; margin-left: 3px; }
  .options-menu { position: absolute; z-index: 40; top: -16px; left: -12px; right: -12px; width: auto; padding: 0; background: #fff; border: 1px solid #d5d5d5; border-radius: 4px; box-shadow: 0 4px 16px #0003; color: #333; overflow: hidden; }
  .colors-dropdown { display: flex; gap: 0; padding: 0; height: 30px; }
  .colors-dropdown .swatch { flex: 1; width: auto; height: 30px; border: 0; }
  .board-option, .delete-option { display: flex; align-items: center; gap: 12px; width: 100%; text-align: left; border: 0; border-radius: 0; background: #fff; padding: 12px 14px; cursor: pointer; font: inherit; }
  .board-option:hover { background: #f2f2f2; }
  .delete-option { border-top: 1px solid #e8e8e8; color: #e3261b; }
  .delete-option:hover { background: #fff0ef; }
  .menu-icon { width: 18px; text-align: center; font-size: 18px; }
  .editor { flex: 1 1 auto; min-height: 0; overflow: auto; padding: 8px 0; }
  :global(.editor .ProseMirror) { min-height: 190px; outline: 0; line-height: 1.4; white-space: pre-wrap; }
  :global(.editor .ProseMirror p) { margin: 0 0 .35em; }
  :global(.editor .checklist-item) { display: flex; align-items: flex-start; gap: 7px; margin: 0 0 .35em; }
  :global(.editor .checklist-item input) { margin-top: .35em; accent-color: #4b74c9; }
  :global(.editor .checklist-item-text) { flex: 1; min-width: 0; }
  :global(.editor .checklist-item[data-checked="true"] .checklist-item-text) { text-decoration: line-through; opacity: .62; }
  :global(.editor .date-chip) { display: inline-block; padding: .08rem .42rem; border-radius: 999px; background: #dbeafe; color: #1e40af; cursor: pointer; white-space: nowrap; font-size: .86em; }
  .chip-popover { position: fixed; z-index: 20; display: grid; gap: 8px; min-width: 220px; padding: 10px; background: white; border: 1px solid #bbb; border-radius: 6px; box-shadow: 0 4px 18px #0003; font-size: 13px; }
  .chip-popover label { display: grid; gap: 3px; } .chip-popover input, .chip-popover select { font: inherit; }
  .chip-actions { display: flex; gap: 5px; justify-content: flex-end; } .chip-actions button { border: 1px solid #bbb; border-radius: 3px; background: #fafafa; padding: 3px 7px; cursor: pointer; }
  .slash-menu { position: fixed; z-index: 30; width: 220px; padding: 5px; background: #fff; border: 1px solid #bbb; border-radius: 7px; box-shadow: 0 5px 20px #0003; font-size: 13px; }
  .slash-title { padding: 4px 8px 5px; color: #666; font-size: 11px; font-weight: 700; text-transform: uppercase; letter-spacing: .06em; }
  .slash-menu button { display: flex; width: 100%; align-items: center; gap: 8px; border: 0; border-radius: 4px; padding: 7px 8px; background: transparent; text-align: left; cursor: pointer; }
  .slash-menu button.selected:not(:disabled), .slash-menu button:hover:not(:disabled) { background: #edf2ff; }
  .slash-menu button.disabled { color: #999; cursor: not-allowed; }
  .slash-icon { width: 20px; text-align: center; font-size: 16px; }
  .slash-copy { display: grid; gap: 1px; } .slash-copy small { color: #777; font-size: 11px; }
  .slash-help { padding: 6px 8px 3px; color: #888; font-size: 10px; border-top: 1px solid #eee; margin-top: 4px; }
  .loading { color: #666; }
  .toolbar { display: flex; align-items: center; gap: 2px; flex: 0 0 28px; min-height: 28px; margin-top: 8px; border-top: 1px solid #0002; padding-top: 5px; }
  .toolbar button { border: 0; border-radius: 3px; background: transparent; min-width: 24px; height: 24px; cursor: pointer; color: #555; font: inherit; }
  .toolbar button:hover, .toolbar button.active { background: #00000018; }
  .toolbar button:disabled { opacity: .4; cursor: not-allowed; }
</style>
