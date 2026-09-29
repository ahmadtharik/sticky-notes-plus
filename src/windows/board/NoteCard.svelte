<script lang="ts">
  import type { Note } from '../../lib/types';

  let { note, onPin, onArchive, onDelete, onOpen }: {
    note: Note;
    onPin: (note: Note) => void;
    onArchive: (note: Note) => void;
    onDelete: (note: Note) => void;
    onOpen: (note: Note) => void;
  } = $props();

  const plain = () => {
    try { return JSON.parse(note.content).plain ?? ''; } catch { return ''; }
  };
</script>

<article class="card" style:background={note.color} role="button" tabindex="0"
  onclick={() => onOpen(note)} onkeydown={(event) => event.key === 'Enter' && onOpen(note)}>
  <div class="body">
    <h2>{note.title || 'Untitled note'}</h2>
    <p>{plain()}</p>
  </div>
  <footer onclick={(event) => event.stopPropagation()}>
    <button aria-label="{note.isPinned ? 'Unpin' : 'Pin'} note" title="Pin" onclick={() => onPin(note)}>⚲</button>
    <button aria-label="Archive note" title="Archive" onclick={() => onArchive(note)}>🗄</button>
    <button aria-label="Delete note" title="Delete" onclick={() => onDelete(note)}>🗑</button>
  </footer>
</article>

<style>
  .card { min-height: 185px; padding: 16px; border-radius: 10px; box-shadow: 0 2px 10px #0002; cursor: pointer; display: flex; flex-direction: column; gap: 12px; transition: transform .12s, box-shadow .12s; }
  .card:hover, .card:focus-visible { transform: translateY(-2px); box-shadow: 0 7px 18px #0003; outline: none; }
  .body { flex: 1; overflow: hidden; }
  h2 { margin: 0 0 8px; font-size: 16px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  p { margin: 0; white-space: pre-wrap; line-height: 1.45; display: -webkit-box; -webkit-line-clamp: 5; -webkit-box-orient: vertical; overflow: hidden; }
  footer { display: flex; justify-content: flex-end; gap: 5px; opacity: 0; transition: opacity .12s; }
  .card:hover footer, .card:focus-within footer { opacity: 1; }
  button { border: 0; border-radius: 5px; padding: 5px 7px; background: #ffffff91; cursor: pointer; }
  button:hover { background: #ffffffe0; }
</style>
