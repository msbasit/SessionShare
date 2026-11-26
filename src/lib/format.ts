import type { SessionSummary } from "./types";

export function formatDate(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function formatMatchSource(src: string | null): string {
  switch (src) {
    case "git":
      return "Auto: Git Remote";
    case "home-relative":
      return "Auto: Home Path";
    case "workspace-scan":
      return "Auto: Workspace Scan";
    case "folder-name":
      return "Auto: Folder Name";
    case "manual":
      return "Manual Map";
    default:
      return "";
  }
}

/**
 * PowerShell command that resumes a session. `claude --resume` only finds a session from inside
 * its project folder, so the command changes directory first when the project is mapped.
 */
export function resumeCommand(sess: SessionSummary): string {
  const resume = `claude --resume ${sess.session_id}`;
  if (!sess.local_project_path) return resume;
  const escaped = sess.local_project_path.replace(/'/g, "''");
  return `Set-Location -LiteralPath '${escaped}'; ${resume}`;
}
