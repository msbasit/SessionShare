<script lang="ts">
  import type { ActivityEntry, MergeConflict, PendingApproval } from "../lib/types";
  import { formatDate } from "../lib/format";
  import MergeView from "./MergeView.svelte";

  interface Props {
    pending: PendingApproval[];
    conflicts: MergeConflict[];
    activities: ActivityEntry[];
    onResolve: (item: PendingApproval, approve: boolean) => void;
    onChanged: () => void;
    onError: (message: string) => void;
  }

  let { pending, conflicts, activities, onResolve, onChanged, onError }: Props = $props();

  let expanded = $state<string | null>(null);
  let merging = $state<string | null>(null);
  let history = $derived(
    activities.filter((a) => a.kind === "conflict" || a.kind === "delete" || a.kind === "merge")
  );

  function backupPath(detail: string): string | null {
    const m = detail.match(/(?:backup at|moved to) (.+?)\)?$/);
    return m ? m[1] : null;
  }

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      onError("Clipboard access failed");
    }
  }
</script>

{#if merging}
  <MergeView
    remoteKey={merging}
    onClose={() => (merging = null)}
    onResolved={() => {
      merging = null;
      onChanged();
    }}
    {onError}
  />
{:else}
<div class="section-header">
  <div>
    <h2>Review</h2>
    <p>
      Files changed differently on both PCs are kept untouched until you merge them. Incoming
      changes to hooks, permissions and command helpers wait for approval because they can run
      code on this PC.
    </p>
  </div>
</div>

<h3>Files to merge ({conflicts.length})</h3>
{#if conflicts.length === 0}
  <div class="empty-card"><p>No conflicting edits.</p></div>
{:else}
  <div class="card-list">
    {#each conflicts as c (c.remote_key)}
      <div class="card row-card">
        <div class="main">
          <div class="title-row">
            <strong class="mono">{c.remote_key}</strong>
            <span class="badge warn">{c.has_base ? "Overlapping edits" : "Differs (no common version)"}</span>
          </div>
          <div class="meta-line">
            <span class="mono">{c.local_path}</span> · Detected {formatDate(c.detected_at)}
          </div>
        </div>
        <button class="btn primary" onclick={() => (merging = c.remote_key)}>Compare &amp; merge</button>
      </div>
    {/each}
  </div>
{/if}

<h3>Waiting for approval ({pending.length})</h3>
{#if pending.length === 0}
  <div class="empty-card"><p>Nothing to approve.</p></div>
{:else}
  <div class="card-list">
    {#each pending as item (item.remote_key)}
      <div class="card">
        <div class="row-card">
          <div class="main">
            <div class="title-row">
              <strong class="mono">{item.remote_key}</strong>
              <span class="badge warn">Pending</span>
            </div>
            <div class="meta-line">{item.summary} · Detected {formatDate(item.detected_at)}</div>
          </div>
          <div class="actions">
            <button
              class="btn"
              onclick={() => (expanded = expanded === item.remote_key ? null : item.remote_key)}
            >
              {expanded === item.remote_key ? "Hide" : "Inspect"}
            </button>
            <button class="btn danger" onclick={() => onResolve(item, false)}>Reject</button>
            <button class="btn primary" onclick={() => onResolve(item, true)}>Approve</button>
          </div>
        </div>
        {#if expanded === item.remote_key}
          <pre class="preview mono">{item.preview}</pre>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<h3>Recent merges, conflicts &amp; deletions ({history.length})</h3>
{#if history.length === 0}
  <div class="empty-card"><p>Nothing recorded yet.</p></div>
{:else}
  <div class="card-list">
    {#each history as act, i (i)}
      {@const backup = backupPath(act.detail)}
      <div class="card row-card">
        <div class="main">
          <div class="title-row">
            <span class="badge" class:warn={act.kind !== "merge"} class:info={act.kind === "merge"}>{act.kind.toUpperCase()}</span>
            <strong class="mono">{act.path}</strong>
          </div>
          <div class="meta-line">{act.detail} · {formatDate(act.timestamp)}</div>
        </div>
        {#if backup}
          <button class="btn" onclick={() => copy(backup)}>Copy backup path</button>
        {/if}
      </div>
    {/each}
  </div>
{/if}
{/if}

<style>
  h3 {
    font-size: 14px;
    margin: 18px 0 10px;
    color: var(--muted);
  }

  .main {
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .actions {
    display: flex;
    gap: 8px;
    flex-shrink: 0;
  }

  .preview {
    margin: 12px 0 0;
    padding: 10px;
    max-height: 320px;
    overflow: auto;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 7px;
    font-size: 12px;
    white-space: pre-wrap;
  }
</style>
