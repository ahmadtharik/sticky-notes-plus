<script lang="ts">
  import { onMount } from 'svelte';
  import { Editor } from '@tiptap/core';
  import StarterKit from '@tiptap/starter-kit';
  import { DateChip } from './chip-probe';

  let host: HTMLDivElement;
  let editor: Editor;
  let jsonOut = '';
  let popover = { open: false, x: 0, y: 0, chipId: '' };
  let activeChip: HTMLElement | null = null;

  const reposition = () => {
    if (!activeChip || !popover.open) return;
    const rect = activeChip.getBoundingClientRect();
    popover = { ...popover, x: rect.left, y: rect.bottom + 8 };
  };

  onMount(() => {
    editor = new Editor({
      element: host,
      extensions: [StarterKit, DateChip],
      content: {
        type: 'doc',
        content: [{ type: 'paragraph', content: [
          { type: 'text', text: 'Finish the report by ' },
          { type: 'dateChip', attrs: { chipId: 'chip-probe-1', label: 'Sep 18 · 5:00 PM' } },
          { type: 'text', text: ' and send it.' },
        ] }],
      },
      onUpdate: ({ editor: current }) => { jsonOut = JSON.stringify(current.getJSON(), null, 2); reposition(); },
    });
    jsonOut = JSON.stringify(editor.getJSON(), null, 2);

    const click = (event: MouseEvent) => {
      const target = (event.target as HTMLElement).closest<HTMLElement>('[data-datechip]');
      if (!target) return;
      activeChip = target;
      const rect = target.getBoundingClientRect();
      popover = { open: true, x: rect.left, y: rect.bottom + 8, chipId: target.dataset.chipId ?? '' };
    };
    host.addEventListener('click', click);
    window.addEventListener('resize', reposition);
    window.addEventListener('scroll', reposition, true);
    return () => {
      host.removeEventListener('click', click);
      window.removeEventListener('resize', reposition);
      window.removeEventListener('scroll', reposition, true);
      editor.destroy();
    };
  });
</script>

<section class="probe">
  <h1>TipTap date-chip spike</h1>
  <p class="hint">Click the chip, then type around it to test reflow.</p>
  <div class="editor-shell"><div bind:this={host} class="editor"></div></div>
  {#if popover.open}
    <div class="popover" style={`left:${popover.x}px; top:${popover.y}px`}>
      <strong>Date chip</strong><br />{popover.chipId}<br />
      <button onclick={() => (popover.open = false)}>Close</button>
    </div>
  {/if}
  <h2>editor.getJSON()</h2>
  <pre>{jsonOut}</pre>
</section>

<style>
  .probe { max-width: 760px; margin: 2rem auto; font: 15px system-ui; color: #202124; }
  .hint { color: #5f6368; }
  .editor-shell { border: 1px solid #c7c9cc; border-radius: 8px; padding: 1rem; min-height: 140px; }
  :global(.editor .ProseMirror) { min-height: 110px; outline: none; line-height: 1.8; }
  :global(.date-chip) { display: inline-block; padding: 0.1rem 0.45rem; border-radius: 999px; background: #dbeafe; color: #1e40af; cursor: pointer; white-space: nowrap; }
  .popover { position: fixed; z-index: 10; padding: .6rem .75rem; background: white; border: 1px solid #aaa; border-radius: 6px; box-shadow: 0 4px 18px #0002; }
  pre { padding: 1rem; background: #f5f6f7; overflow: auto; font-size: 12px; }
  button { margin-top: .4rem; }
</style>
