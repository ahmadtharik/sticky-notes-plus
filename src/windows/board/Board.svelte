<script lang="ts">
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { createNote, listNotes, openNoteWindow, softDelete, updateNoteMeta } from '../../lib/api';
  import type { Note } from '../../lib/types';
  import NoteCard from './NoteCard.svelte';

  let notes = $state<Note[]>([]);
  let search = $state('');
  let includeArchived = $state(false);
  let loading = $state(true);
  let error = $state('');
  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  let refreshTimer: ReturnType<typeof setTimeout> | undefined;
  let loadSerial = 0;

  async function load(showLoading = false) {
    const serial = ++loadSerial;
    if (showLoading) loading = true;
    error = '';
    try {
      const nextNotes = await listNotes({ includeArchived, search });
      // A slower request must not overwrite a newer search/event refresh.
      if (serial === loadSerial) notes = nextNotes;
    }
    catch (reason) { if (serial === loadSerial) error = String(reason); }
    finally { if (serial === loadSerial) loading = false; }
  }

  function scheduleLoad() {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(load, 250);
  }

  function scheduleRefresh() {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => { void load(false); }, 75);
  }

  async function makeNote() {
    const note = await createNote();
    await openNoteWindow(note.id);
  }

  onMount(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen('notes-changed', () => {
      if (!disposed) scheduleRefresh();
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    }).catch((reason) => { if (!disposed) console.error('Unable to listen for note changes', reason); });
    void load(true);
    return () => {
      disposed = true;
      unlisten?.();
      clearTimeout(searchTimer);
      clearTimeout(refreshTimer);
    };
  });
</script>

<main>
  <header>
    <div><h1>Sticky Notes+</h1><p>{notes.length} note{notes.length === 1 ? '' : 's'}</p></div>
    <input aria-label="Search notes" bind:value={search} oninput={scheduleLoad} placeholder="Search notes" />
    <label class="archive"><input type="checkbox" bind:checked={includeArchived} onchange={() => void load(false)} /> Include archived</label>
    <button class="new" onclick={makeNote}>New note</button>
  </header>

  {#if error}
    <p class="error">Could not load notes: {error}</p>
  {:else if loading}
    <p class="empty">Loading notes…</p>
  {:else if notes.length === 0}
    <p class="empty">No notes here yet. Create one to get started.</p>
  {:else}
    <section class="grid" aria-label="Notes">
      {#each notes as note (note.id)}
        <NoteCard {note}
          onOpen={(n) => openNoteWindow(n.id)}
          onPin={async (n) => { await updateNoteMeta(n.id, { isPinned: !n.isPinned }); }}
          onArchive={async (n) => { await updateNoteMeta(n.id, { isArchived: true }); }}
          onDelete={async (n) => { await softDelete(n.id); }} />
      {/each}
    </section>
  {/if}
</main>

<style>
  :global(body) { margin: 0; color: #202124; background: #f5f6f8; font-family: system-ui, sans-serif; }
  main { padding: 28px; max-width: 1400px; margin: auto; }
  header { display: flex; align-items: center; gap: 14px; margin-bottom: 28px; }
  h1 { margin: 0; font-size: 26px; } header p { margin: 2px 0 0; color: #667085; font-size: 13px; }
  input[type="text"], input:not([type]) { min-width: 230px; padding: 10px 12px; border: 1px solid #d0d5dd; border-radius: 7px; font: inherit; }
  header > input { margin-left: auto; }
  .archive { font-size: 14px; white-space: nowrap; }
  .new { padding: 10px 14px; color: white; background: #2e6be6; border: 0; border-radius: 7px; font: inherit; cursor: pointer; white-space: nowrap; }
  .new:hover { background: #205acb; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 18px; }
  .empty, .error { margin-top: 80px; text-align: center; color: #667085; } .error { color: #b42318; }
  @media (max-width: 720px) { header { flex-wrap: wrap; } header > input { margin-left: 0; flex: 1 1 100%; } }
</style>
