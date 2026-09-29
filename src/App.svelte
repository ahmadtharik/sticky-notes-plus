<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { onMount } from 'svelte';
  import Probe from './Probe.svelte';
  import ChipProbe from './probes/chip-probe.svelte';
  import Board from './windows/board/Board.svelte';
  import NoteWindow from './windows/note/NoteWindow.svelte';

  // The Vite browser preview does not provide Tauri's window metadata. Keep
  // the Tauri-only lookup isolated so URL-based routes remain testable there.
  let windowLabel = '';
  try {
    windowLabel = getCurrentWindow().label;
  } catch {
    // Running in a regular browser (rather than a Tauri webview).
  }

  const browserSearch = typeof window !== 'undefined' ? window.location.search : '';
  // Secondary webviews can start before Tauri has injected current-window
  // metadata. The Rust builder also passes the note id in the URL so routing
  // remains deterministic during that early startup window.
  const routeNoteId = typeof window !== 'undefined'
    ? new URLSearchParams(browserSearch).get('note') ?? ''
    : '';
  const isProbe = windowLabel.startsWith('probe-');
  const isChipProbe = windowLabel.startsWith('chip-probe-') || browserSearch.includes('chip-probe');
  const isBoard = windowLabel === 'board';
  const isNote = windowLabel.startsWith('note-') || routeNoteId.length > 0;
  const noteId = windowLabel.startsWith('note-') ? windowLabel.slice(5) : routeNoteId;

  let info: { version: string; app_data_dir: string } | null = null;
  onMount(async () => {
    if (!isProbe && !isBoard && !isChipProbe && !isNote) {
      info = (await invoke('get_app_info')) as {
        version: string;
        app_data_dir: string;
      };
    }
  });
</script>

{#if isProbe}
  <Probe />
{:else if isChipProbe}
  <ChipProbe />
{:else if isBoard}
  <Board />
{:else if isNote}
  <NoteWindow noteId={noteId} />
{:else if info}
  <h1>Sticky Notes+ {info.version}</h1>
  <p>data dir: {info.app_data_dir}</p>
{:else}
  <p>loading…</p>
{/if}
