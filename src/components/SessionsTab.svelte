<script lang="ts">
  import type { SessionSummary } from "../lib/types";
  import { formatBytes, formatDate, resumeCommand } from "../lib/format";

  interface Props {
    sessions: SessionSummary[];
    onError: (message: string) => void;
  }

  let { sessions, onError }: Props = $props();

  let searchQuery = $state("");
  let copiedId = $state<string | null>(null);

  let filtered = $derived.by(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) return sessions;
    return sessions.filter(
      (s) =>
        s.title.toLowerCase().includes(q) ||
        s.project_name.toLowerCase().includes(q) ||
        s.session_id.toLowerCase().includes(q)
    );
  });

  async function copyResume(sess: SessionSummary) {
    try {
      await navigator.clipboard.writeText(resumeCommand(sess));
      copiedId = sess.session_id;
      setTimeout(() => {
        if (copiedId === sess.session_id) copiedId = null;
      }, 1800);
    } catch {
      onError("Clipboard access failed");
    }
  }
</script>

<div class="section-header">
  <div>
    <h2>Sessions</h2>
    <p>
      Search conversations across both PCs. "Copy resume" copies a PowerShell command that opens
      the project folder and resumes the session.
    </p>
  </div>
  <input
    class="text-input search"
    type="text"
    placeholder="Search sessions or projects..."
    bind:value={searchQuery}
  />
</div>

{#if filtered.length === 0}
  <div class="empty-card">
    <p>No sessions match your filter.</p>
  </div>
{:else}
  <div class="card-list">
    {#each filtered as sess (sess.project_id + "/" + sess.session_id)}
      <div class="card row-card">
        <div class="main">
          <div class="title-row">
            <strong>{sess.title}</strong>
            <span class="badge info">{sess.project_name}</span>
            {#if sess.in_local && sess.in_remote}
              <span class="badge ok">Local + Drive</span>
            {:else if sess.in_local}
              <span class="badge">Local Only</span>
            {:else}
              <span class="badge warn">Drive Only</span>
            {/if}
          </div>
          <div class="meta-line">
            <span class="mono">{sess.session_id}</span> · {sess.line_count} events ·
            {formatBytes(sess.size_bytes)} · Updated {formatDate(sess.last_modified)}
          </div>
        </div>
        <button
          class="btn"
          title={resumeCommand(sess)}
          disabled={!sess.in_local}
          onclick={() => copyResume(sess)}
        >
          {copiedId === sess.session_id ? "Copied!" : "Copy resume"}
        </button>
      </div>
    {/each}
  </div>
{/if}

<style>
  .main {
    min-width: 0;
  }

  .search {
    width: 260px;
  }
</style>
