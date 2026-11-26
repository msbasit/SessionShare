<script lang="ts">
  import type { ProjectView } from "../lib/types";
  import { formatMatchSource } from "../lib/format";

  interface Props {
    projects: ProjectView[];
    onRefresh: () => void;
    onMap: (proj: ProjectView) => void;
    onToggle: (proj: ProjectView) => void;
  }

  let { projects, onRefresh, onMap, onToggle }: Props = $props();
</script>

<div class="section-header">
  <div>
    <h2>Projects</h2>
    <p>
      Projects with the same Git remote auto-link across PCs. Toggle sync per project or map
      unlinked remote projects to a local folder.
    </p>
  </div>
  <button class="btn" onclick={onRefresh}>Refresh</button>
</div>

{#if projects.length === 0}
  <div class="empty-card">
    <p>No Claude Code projects found yet. Run Claude Code in a folder or sync from your other PC.</p>
  </div>
{:else}
  <div class="card-list">
    {#each projects as proj (proj.canonical_id)}
      <div class="card row-card">
        <div class="main">
          <div class="title-row">
            <strong>{proj.display_name}</strong>
            {#if proj.needs_mapping}
              <span class="badge warn">Needs Local Mapping</span>
            {:else if proj.sync_enabled}
              <span class="badge ok">Synced</span>
            {:else}
              <span class="badge">Local Only</span>
            {/if}
            {#if formatMatchSource(proj.mapping_source)}
              <span class="badge">{formatMatchSource(proj.mapping_source)}</span>
            {/if}
            {#if proj.remote_machines.length > 0}
              <span class="badge info">Also on: {proj.remote_machines.join(", ")}</span>
            {/if}
          </div>
          <div class="meta-line mono">{proj.local_path ?? "Not mapped on this PC yet"}</div>
          {#if proj.git_remote}
            <div class="meta-line">Git: <span class="mono">{proj.git_remote}</span></div>
          {/if}
          <div class="meta-line">
            Local sessions: {proj.local_session_count} · Synced drive sessions: {proj.remote_session_count}
          </div>
        </div>
        <div class="actions">
          <button class="btn" onclick={() => onMap(proj)}>
            {proj.local_path ? "Change Folder" : "Map Local Folder"}
          </button>
          <label class="switch">
            <input
              type="checkbox"
              checked={proj.sync_enabled}
              disabled={proj.needs_mapping}
              onchange={() => onToggle(proj)}
            />
            <span>Sync</span>
          </label>
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  .main {
    min-width: 0;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-shrink: 0;
  }
</style>
