<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  let info: { version: string; app_data_dir: string } | null = null;
  onMount(async () => {
    info = (await invoke('get_app_info')) as { version: string; app_data_dir: string };
  });
</script>

{#if info}
  <h1>Sticky Notes+ {info.version}</h1>
  <p>data dir: {info.app_data_dir}</p>
{:else}
  <p>loading…</p>
{/if}