<script lang="ts">
  import type { SyncProgress } from "../lib/types";

  interface Props {
    progress: SyncProgress | null;
  }

  let { progress }: Props = $props();
</script>

<section class="sync-progress" aria-label="Sync progress">
  <div class="heading">
    <div>
      <h2>Sync in progress</h2>
      <p>{progress?.phase ?? "Preparing sync"}</p>
    </div>
    <span class="checked">{progress?.checked ?? 0} files checked</span>
  </div>

  {#if progress && progress.total > 0}
    <div class="batch">
      <progress
        value={progress.completed}
        max={progress.total}
        aria-label="Files checked in current group"
      ></progress>
      <span>{progress.completed} / {progress.total} in this group</span>
    </div>
  {/if}

  {#if progress?.current_item || progress?.recent.length}
    <div class="details">
      {#if progress.current_item}
        <div class="detail">
          <strong>Checking</strong>
          <div class="path mono" title={progress.current_item}>{progress.current_item}</div>
          {#if progress.upcoming.length}<span class="hint">Next in this group</span>{/if}
          {#each progress.upcoming as item}
            <div class="next mono" title={item}>{item}</div>
          {/each}
        </div>
      {/if}
      {#if progress.recent.length}
        <div class="detail">
          <strong>Recent changes</strong>
          {#each progress.recent as item}
            <div class="change">
              <span class="kind">{item.kind}</span>
              <span class="path mono" title="{item.path}: {item.detail}">{item.path}</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</section>

<style>
  .sync-progress {
    margin-bottom: 24px;
    padding: 16px 18px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
  }

  .heading {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }

  h2 {
    margin: 0;
    font-size: 15px;
  }

  p, .checked, .batch, .hint {
    margin: 3px 0 0;
    color: var(--muted);
    font-size: 12px;
  }

  .checked {
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  .batch {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 12px;
    font-variant-numeric: tabular-nums;
  }

  progress {
    flex: 1;
    height: 6px;
    accent-color: var(--accent-strong);
  }

  .details {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 14px 24px;
    margin-top: 14px;
  }

  .detail {
    min-width: 0;
    font-size: 12px;
  }

  .detail strong {
    display: block;
    margin-bottom: 5px;
  }

  .path, .next {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .next {
    color: var(--muted);
  }

  .hint {
    display: block;
  }

  .change {
    display: flex;
    gap: 8px;
    margin-top: 3px;
  }

  .kind {
    color: var(--accent);
    font-weight: 600;
    min-width: 54px;
    text-transform: capitalize;
  }
</style>
