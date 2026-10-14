/**
 * Headless stand-in for `src/components/NetworkSettings.tsx` (`vite.config.ts`
 * next to this file aliases it in). The desktop panel drives Clash, which a
 * server doesn't have; here the query route is a list of exit proxies, and
 * the server picks one at random for every Apple request.
 */
import { useEffect, useState } from "react";
import { Globe } from "lucide-react";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { Label } from "@/components/ui/label";
import { saveSettings } from "@/lib/store";
import type { NetworkSettings as DesktopNetwork } from "@/lib/types";

/** `proxies` is headless-only, so the shared desktop type doesn't list it. */
type Network = DesktopNetwork & { proxies?: string[] };

function entries(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
}

export function NetworkSettings({ network }: { network: Network; running: boolean; route: string | null }) {
  const saved = (network.proxies ?? []).join("\n");
  const [draft, setDraft] = useState(saved);
  const [failed, setFailed] = useState(false);
  useEffect(() => setDraft(saved), [saved]);

  const count = network.proxies?.length ?? 0;
  const summary = count > 0 ? `Random of ${count} proxies` : "Sidecar default";

  const save = async () => {
    const proxies = entries(draft);
    if (proxies.join("\n") === saved) return;
    const next: Network = { ...network, proxies };
    setFailed(!(await saveSettings({ network: next })));
  };

  return (
    <CollapsiblePanel
      id="network"
      icon={<Globe className="size-4" />}
      title="Query route"
      subtitle="Exit proxies for Apple queries"
      summary={summary}
    >
      <div className="field-group">
        <Label htmlFor="network-proxies" className="control-label">
          Proxy list
        </Label>
        <textarea
          id="network-proxies"
          rows={5}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={failed}
          className="w-full min-w-0 resize-y rounded-xl border border-border/70 bg-background/40 px-3 py-2 font-mono text-xs leading-5 outline-none transition-colors select-text placeholder:text-muted-foreground hover:border-primary/35 focus-visible:border-primary/60 aria-invalid:border-destructive"
          placeholder={"http://user:pass@203.0.113.7:8080\nsocks5://203.0.113.8:1080"}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onBlur={() => void save()}
        />
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {failed
            ? "Not saved: the activity log has the reason."
            : "One per line: http://, https:// or socks5://, with an explicit port. Every request uses a random one, " +
              "each with its own warmed browser session. socks5 cannot carry a username/password (browser limit). " +
              "Empty: the sidecar's SIDECAR_PROXY, or a direct connection."}
        </p>
      </div>
    </CollapsiblePanel>
  );
}
