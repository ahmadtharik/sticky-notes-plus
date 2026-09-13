<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { onMount } from 'svelte';
  import Probe from './Probe.svelte';

  // Probe windows (labels "probe-N") render the mock note-card workload;
  // the board keeps the info view until real views land.
  const isProbe = getCurrentWindow().label.startsWith('probe-');

  let info: { version: string; app_data_dir: string } | null = null;
  onMount(async () => {
    if (!isProbe) {
      info = (await invoke('get_app_info')) as {
        version: string;
        app_data_dir: string;
      };
    }
  });
</script>

{#if isProbe}
  <Probe />
{:else if info}
  <h1>Sticky Notes+ {info.version}</h1>
  <p>data dir: {info.app_data_dir}</p>
{:else}
  <p>loading…</p>
{/if}