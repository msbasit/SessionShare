<script lang="ts">
  import type { GlobalSyncOptions } from "../lib/types";

  interface Props {
    options: GlobalSyncOptions;
    onToggle: (key: keyof GlobalSyncOptions) => void;
  }

  let { options, onToggle }: Props = $props();

  const rows: [keyof GlobalSyncOptions, string, string][] = [
    [
      "desktop_sessions",
      "Claude Desktop Sessions",
      "Syncs Claude Desktop Code tab sidebar sessions (local_*.json) with automatic path remapping",
    ],
    ["project_memory", "Project Memory (memory/)", "Per-project Claude auto-memory files"],
    ["skills", "Skills (skills/)", "Custom reusable Claude Code skills"],
    ["agents", "Agents (agents/)", "Custom subagent definitions"],
    ["commands", "Slash Commands (commands/)", "Custom slash command prompts"],
    ["plans", "Plans (plans/)", "Saved Claude Code plan documents"],
    ["rules", "Rules (rules/)", "Custom Claude Code rule files"],
    ["hooks", "Hooks (hooks/)", "Hook scripts — incoming changes need your approval (Review tab)"],
    ["claude_md", "Global CLAUDE.md", "Global instructions across all projects"],
    [
      "settings_json",
      "settings.json",
      "Paths remapped automatically; local-only keys (e.g. env) never leave this PC",
    ],
    ["history_jsonl", "history.jsonl", "Global prompt history with automatic project path remapping"],
  ];
</script>

<div class="section-header">
  <div>
    <h2>Global Claude Code Assets</h2>
    <p>Choose which portable Claude Code folders and files sync across your PCs.</p>
  </div>
</div>

<div class="card-list">
  {#each rows as [key, label, desc] (key)}
    <div class="card row-card">
      <div>
        <strong>{label}</strong>
        <div class="meta-line">{desc}</div>
      </div>
      <label class="switch">
        <input type="checkbox" checked={options[key]} onchange={() => onToggle(key)} />
        <span>Enabled</span>
      </label>
    </div>
  {/each}
</div>
