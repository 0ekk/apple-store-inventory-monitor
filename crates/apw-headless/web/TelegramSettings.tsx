/**
 * Headless-only Telegram alert panel. The desktop UI has no slot for it, so
 * the headless `NetworkSettings.tsx` (aliased in by `vite.config.ts`) renders
 * it just above the query-route panel, below the Bark settings.
 */
import { useEffect, useState, useSyncExternalStore } from "react";
import { Send } from "lucide-react";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { saveSettings, watcherStore } from "@/lib/store";
import type { Settings } from "@/lib/types";

/** Headless-only, so the shared desktop `Settings` type doesn't list it. */
type Telegram = { botToken: string; chatId: string };
const EMPTY: Telegram = { botToken: "", chatId: "" };

export function TelegramSettings() {
  const ui = useSyncExternalStore(watcherStore.subscribe, watcherStore.getSnapshot);
  const saved = (ui.settings as Settings & { telegram?: Telegram }).telegram ?? EMPTY;
  const [draft, setDraft] = useState(saved);
  const [failed, setFailed] = useState(false);
  useEffect(() => setDraft(saved), [saved]);

  const next = { botToken: draft.botToken.trim(), chatId: draft.chatId.trim() };
  // Saved only once both are filled in (or both cleared), so leaving the
  // first field for the second doesn't hit the server's both-or-neither check.
  const half = !next.botToken !== !next.chatId;

  const save = async () => {
    if (half || (next.botToken === saved.botToken && next.chatId === saved.chatId)) return;
    const patch: Partial<Settings> & { telegram: Telegram } = { telegram: next };
    setFailed(!(await saveSettings(patch)));
  };

  return (
    <CollapsiblePanel
      id="telegram"
      icon={<Send className="size-4" />}
      title="Telegram"
      subtitle="Alerts through your own bot"
      summary={saved.chatId ? `Chat ${saved.chatId}` : "Server default"}
    >
      <div className="field-group">
        <Label htmlFor="telegram-token" className="control-label">
          Bot token
        </Label>
        <Input
          id="telegram-token"
          type="password"
          autoComplete="off"
          spellCheck={false}
          className="control-surface select-text"
          placeholder="123456:ABC-DEF..."
          value={draft.botToken}
          onChange={(event) => setDraft({ ...draft, botToken: event.target.value })}
          onBlur={() => void save()}
        />
      </div>
      <div className="mt-3 field-group">
        <Label htmlFor="telegram-chat" className="control-label">
          Chat ID
        </Label>
        <Input
          id="telegram-chat"
          autoComplete="off"
          spellCheck={false}
          className="control-surface select-text"
          placeholder="-1001234567890 or @channel"
          value={draft.chatId}
          onChange={(event) => setDraft({ ...draft, chatId: event.target.value })}
          onBlur={() => void save()}
        />
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {half
            ? "Fill in both to save."
            : failed
              ? "Not saved: the activity log has the reason."
              : "Send the bot a message first, then read chat.id from " +
                "api.telegram.org/bot<token>/getUpdates. The test button above sends to Bark and Telegram. " +
                "Both empty: the server's APW_TELEGRAM_BOT_TOKEN / APW_TELEGRAM_CHAT_ID, if set."}
        </p>
      </div>
    </CollapsiblePanel>
  );
}
