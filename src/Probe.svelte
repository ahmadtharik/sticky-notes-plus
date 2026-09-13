<script lang="ts">
  // Probe content: 20 mock note cards mimicking the app's content model
  // (title line, text + date chip, checklist rows). Same workload as the
  // Slint probe, for a fair per-window RAM comparison.
  const notes = Array.from({ length: 20 }, (_, i) => i + 1);
</script>

<div class="probe">
  {#each notes as n}
    <div class="card">
      <div class="title">Note {n}</div>
      <div class="line">
        <span>Finish the quarterly report by</span>
        <span class="chip">📅 Fri, Sep 18 · 5:00 PM</span>
      </div>
      <label class="item"><input type="checkbox" /> Review draft</label>
      <label class="item"><input type="checkbox" checked /> Send to team</label>
    </div>
  {/each}
</div>

<style>
  .probe {
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .card {
    background: #fff9b1;
    border-radius: 4px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-family: 'Segoe UI', sans-serif;
  }
  .title {
    font-weight: 600;
    font-size: 15px;
  }
  .line {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .chip {
    background: #dce8ff;
    border-radius: 11px;
    padding: 2px 10px;
    font-size: 12px;
    white-space: nowrap;
  }
  .item {
    display: flex;
    gap: 6px;
    align-items: center;
  }
</style>