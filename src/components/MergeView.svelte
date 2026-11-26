<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../lib/api";
  import type { Chunk, ConflictView } from "../lib/types";

  interface Props {
    remoteKey: string;
    onClose: () => void;
    onResolved: () => void;
    onError: (message: string) => void;
  }

  let { remoteKey, onClose, onResolved, onError }: Props = $props();

  type Choice = "local" | "remote" | "local_remote" | "remote_local";
  const STABLE_PREVIEW = 3;

  let view = $state<ConflictView | null>(null);
  let loadError = $state<string | null>(null);
  let picks = $state<(Choice | null)[]>([]);
  let expanded = $state<Record<number, boolean>>({});
  let showBase = $state<Record<number, boolean>>({});
  let manual = $state(false);
  let manualText = $state("");
  let saving = $state(false);

  async function load() {
    loadError = null;
    try {
      view = await api.getConflictView(remoteKey);
      resetPicks();
      manual = false;
    } catch (err) {
      loadError = String(err);
    }
  }

  function resetPicks() {
    picks = view ? view.chunks.map((c) => (c.stable ? null : c.auto)) : [];
  }

  function pickAll(choice: Choice) {
    if (!view) return;
    picks = view.chunks.map((c) => (c.stable ? null : choice));
    manual = false;
  }

  function choose(i: number, choice: Choice) {
    picks[i] = choice;
    manual = false;
  }

  function append(out: string, lines: string[]): string {
    for (const line of lines) {
      if (out && !out.endsWith("\n")) out += "\n";
      out += line;
    }
    return out;
  }

  function linesFor(chunk: Chunk, pick: Choice | null): string[] | null {
    switch (pick) {
      case "local":
        return chunk.local;
      case "remote":
        return chunk.remote;
      case "local_remote":
        return [...chunk.local, ...chunk.remote];
      case "remote_local":
        return [...chunk.remote, ...chunk.local];
      default:
        return null;
    }
  }

  function compose(v: ConflictView, p: (Choice | null)[]): string {
    let out = "";
    v.chunks.forEach((chunk, i) => {
      if (chunk.stable) {
        out = append(out, chunk.local);
        return;
      }
      const chosen = linesFor(chunk, p[i]);
      if (chosen) {
        out = append(out, chosen);
      } else {
        out = append(out, ["<<<<<<< This PC\n", ...chunk.local]);
        out = append(out, ["=======\n", ...chunk.remote]);
        out = append(out, [">>>>>>> Other PC\n"]);
      }
    });
    return out;
  }

  let changeCount = $derived(view ? view.chunks.filter((c) => !c.stable).length : 0);
  let unresolved = $derived(
    view ? view.chunks.filter((c, i) => !c.stable && picks[i] == null).length : 0
  );
  let result = $derived(manual ? manualText : view ? compose(view, picks) : "");
  let jsonError = $derived.by(() => {
    if (view?.kind !== "json" || (!manual && unresolved > 0)) return null;
    try {
      JSON.parse(result);
      return null;
    } catch (e) {
      return String(e);
    }
  });
  let canSave = $derived(!!view && !saving && (manual || unresolved === 0) && !jsonError);

  // First line number of each chunk on both sides.
  let starts = $derived.by(() => {
    const out: { local: number; remote: number }[] = [];
    let l = 1;
    let r = 1;
    for (const c of view?.chunks ?? []) {
      out.push({ local: l, remote: r });
      l += c.local.length;
      r += c.remote.length;
    }
    return out;
  });

  function display(line: string): string {
    const t = line.replace(/\r?\n$/, "");
    return t === "" ? "\u00a0" : t;
  }

  function isChosen(pick: Choice | null, side: "local" | "remote"): boolean {
    return pick === side || pick === "local_remote" || pick === "remote_local";
  }

  async function save() {
    if (!view || !canSave) return;
    saving = true;
    try {
      await api.resolveConflict(view.remote_key, view.local_hash, view.remote_hash, result);
      onResolved();
    } catch (err) {
      onError(String(err));
      if (String(err).includes("changed on one of the PCs")) await load();
    } finally {
      saving = false;
    }
  }

  onMount(load);
</script>

<div class="section-header">
  <div>
    <h2>Merge <span class="mono">{remoteKey}</span></h2>
    {#if view}
      <p>
        <span class="mono">{view.local_path}</span>
        {#if view.has_base}
          · Changes made on only one PC were merged automatically; pick a version where both PCs
          edited the same lines.
        {:else}
          · No common version is known yet (first sync), so choose a version for each difference.
        {/if}
      </p>
    {/if}
  </div>
  <button class="btn" onclick={onClose}>← Back</button>
</div>

{#if loadError}
  <div class="empty-card">
    <p>{loadError}</p>
    <button class="btn" onclick={load}>Retry</button>
  </div>
{:else if !view}
  <div class="empty-card"><p>Loading...</p></div>
{:else}
  <div class="toolbar">
    <span class="badge" class:warn={unresolved > 0} class:ok={unresolved === 0}>
      {unresolved === 0
        ? `All ${changeCount} difference(s) resolved`
        : `${unresolved} of ${changeCount} difference(s) need a decision`}
    </span>
    <div class="spacer"></div>
    <button class="btn" onclick={() => pickAll("local")}>Use this PC for all</button>
    <button class="btn" onclick={() => pickAll("remote")}>Use other PC for all</button>
    {#if view.has_base}
      <button class="btn" onclick={() => { resetPicks(); manual = false; }}>Reset to automatic</button>
    {/if}
  </div>

  <div class="diff mono">
    <div class="diff-head">
      <div>This PC</div>
      <div>Other PC (synced drive)</div>
    </div>

    {#each view.chunks as chunk, i (i)}
      {#if chunk.stable}
        {@const collapsible = chunk.local.length > STABLE_PREVIEW * 2 + 1 && !expanded[i]}
        {#each chunk.local as line, n (n)}
          {#if !collapsible || n < STABLE_PREVIEW || n >= chunk.local.length - STABLE_PREVIEW}
            <div class="row">
              <div class="cell"><span class="ln">{starts[i].local + n}</span>{display(line)}</div>
              <div class="cell"><span class="ln">{starts[i].remote + n}</span>{display(line)}</div>
            </div>
          {:else if n === STABLE_PREVIEW}
            <button class="fold" onclick={() => (expanded[i] = true)}>
              ⋯ {chunk.local.length - STABLE_PREVIEW * 2} unchanged lines
            </button>
          {/if}
        {/each}
      {:else}
        <div class="chunk" class:open={picks[i] == null}>
          <div class="chunk-bar">
            {#if chunk.auto}
              <span class="badge info">
                Auto: only {chunk.auto === "local" ? "this PC" : "the other PC"} changed this
              </span>
            {:else}
              <span class="badge warn">
                {view.has_base ? "Changed on both PCs" : "Different"}
              </span>
            {/if}
            <div class="spacer"></div>
            {#each [["local", "This PC"], ["remote", "Other PC"], ["local_remote", "Both (this PC first)"], ["remote_local", "Both (other first)"]] as [choice, label] (choice)}
              <button
                class="btn small"
                class:primary={picks[i] === choice}
                onclick={() => choose(i, choice as Choice)}>{label}</button
              >
            {/each}
            {#if chunk.base && chunk.base.length > 0}
              <button class="btn small" onclick={() => (showBase[i] = !showBase[i])}>
                {showBase[i] ? "Hide original" : "Original"}
              </button>
            {/if}
          </div>
          {#if showBase[i] && chunk.base}
            <div class="base">
              {#each chunk.base as line, n (n)}<div class="cell">{display(line)}</div>{/each}
            </div>
          {/if}
          <div class="sides">
            {#each ["local", "remote"] as side (side)}
              {@const lines = side === "local" ? chunk.local : chunk.remote}
              {@const start = side === "local" ? starts[i].local : starts[i].remote}
              <div
                class="side {side}"
                class:chosen={isChosen(picks[i], side as "local" | "remote")}
                class:dropped={picks[i] != null && !isChosen(picks[i], side as "local" | "remote")}
              >
                {#if lines.length === 0}
                  <div class="cell empty">(no lines)</div>
                {:else}
                  {#each lines as line, n (n)}
                    <div class="cell"><span class="ln">{start + n}</span>{display(line)}</div>
                  {/each}
                {/if}
              </div>
            {/each}
          </div>
        </div>
      {/if}
    {/each}
  </div>

  <div class="result">
    <div class="result-head">
      <strong>Merged result</strong>
      <span class="meta-line">
        {manual
          ? "Edited by hand."
          : "Updates as you choose; you can also edit it directly."}
      </span>
      <div class="spacer"></div>
      {#if manual}
        <button class="btn small" onclick={() => (manual = false)}>Discard manual edits</button>
      {/if}
    </div>
    <textarea
      class="text-input mono"
      spellcheck="false"
      rows="14"
      value={result}
      oninput={(e) => {
        manualText = e.currentTarget.value;
        manual = true;
      }}
    ></textarea>
    {#if jsonError}
      <div class="meta-line error">Not valid JSON: {jsonError}</div>
    {/if}
  </div>

  <div class="footer">
    <button class="btn" onclick={onClose}>Cancel</button>
    <button class="btn primary" disabled={!canSave} onclick={save}>
      {saving ? "Saving..." : "Save merged version to both PCs"}
    </button>
  </div>
{/if}

<style>
  .toolbar,
  .chunk-bar,
  .result-head,
  .footer {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .toolbar {
    margin-bottom: 10px;
  }

  .spacer {
    flex: 1;
  }

  .btn.small {
    padding: 3px 8px;
    font-size: 12px;
  }

  .diff {
    border: 1px solid var(--border);
    border-radius: 10px;
    overflow: hidden;
    font-size: 12px;
    background: var(--surface);
  }

  .diff-head,
  .row,
  .sides {
    display: grid;
    grid-template-columns: 1fr 1fr;
  }

  .diff-head > div {
    padding: 6px 10px;
    font-family: "Segoe UI", sans-serif;
    font-weight: 600;
    color: var(--muted);
    border-bottom: 1px solid var(--border);
  }

  .diff-head > div + div,
  .row > .cell + .cell,
  .side + .side {
    border-left: 1px solid var(--border);
  }

  .cell {
    padding: 1px 10px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    min-height: 18px;
  }

  .ln {
    display: inline-block;
    width: 34px;
    margin-right: 8px;
    color: var(--muted);
    opacity: 0.6;
    text-align: right;
    user-select: none;
  }

  .row .cell {
    color: var(--muted);
  }

  .fold {
    width: 100%;
    background: var(--bg);
    border: none;
    border-top: 1px solid var(--border);
    border-bottom: 1px solid var(--border);
    color: var(--accent);
    padding: 3px;
    font-size: 12px;
  }

  .chunk {
    border-top: 1px solid var(--border);
    border-bottom: 1px solid var(--border);
    background: rgba(56, 189, 248, 0.04);
  }

  .chunk.open {
    background: rgba(251, 191, 36, 0.06);
  }

  .chunk-bar {
    padding: 6px 10px;
    font-family: "Segoe UI", sans-serif;
  }

  .base {
    margin: 0 10px 6px;
    border: 1px dashed var(--border);
    border-radius: 6px;
    color: var(--muted);
  }

  .side {
    padding: 2px 0 6px;
    border-left: 3px solid transparent;
  }

  .side.local .cell {
    background: rgba(56, 189, 248, 0.08);
  }

  .side.remote .cell {
    background: rgba(167, 139, 250, 0.1);
  }

  .side.chosen {
    border-left-color: var(--success);
  }

  .side.dropped {
    opacity: 0.45;
  }

  .side.dropped .cell {
    text-decoration: line-through;
  }

  .cell.empty {
    color: var(--muted);
    font-style: italic;
    background: transparent !important;
  }

  .result {
    margin-top: 14px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  textarea {
    resize: vertical;
    font-size: 12px;
    line-height: 1.45;
  }

  .error {
    color: var(--danger);
  }

  .footer {
    justify-content: flex-end;
    margin-top: 12px;
  }
</style>
