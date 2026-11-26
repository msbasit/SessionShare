<script lang="ts">
  import type { ActivityEntry } from "../lib/types";
  import { formatDate } from "../lib/format";

  interface Props {
    activities: ActivityEntry[];
    onRefresh: () => void;
  }

  let { activities, onRefresh }: Props = $props();

  function badgeClass(kind: string): string {
    switch (kind) {
      case "push":
      case "pull":
        return "ok";
      case "merge":
      case "auto-map":
        return "info";
      case "conflict":
      case "delete":
        return "warn";
      default:
        return "";
    }
  }
</script>

<div class="section-header">
  <div>
    <h2>Sync Activity</h2>
    <p>Recent pushes, pulls, merges, deletions, and conflict backups.</p>
  </div>
  <button class="btn" onclick={onRefresh}>Refresh</button>
</div>

{#if activities.length === 0}
  <div class="empty-card"><p>No sync activity recorded yet.</p></div>
{:else}
  <div class="card-list">
    {#each activities as act, i (i)}
      <div class="card row">
        <span class="badge {badgeClass(act.kind)}">{act.kind.toUpperCase()}</span>
        <div class="body">
          <div class="mono">{act.path}</div>
          <div class="meta-line">{act.detail} · {formatDate(act.timestamp)}</div>
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  .row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .body {
    min-width: 0;
    overflow-wrap: anywhere;
  }
</style>
