'use client';

import { useState } from 'react';
import { useRouter } from 'next/navigation';
import {
  AlertCircle,
  ChevronsLeft,
  ChevronsRight,
  History,
  MessageCircleQuestion,
  Sparkles,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useAssistant } from '@/contexts/AssistantContext';
import { AssistantCard } from './AssistantCard';
import { AskBox } from './AskBox';
import { NoteBar } from './NoteBar';

const MODE_ORDER = ['manual', 'gated', 'continuous'] as const;
const MODE_LABELS: Record<(typeof MODE_ORDER)[number], string> = {
  manual: 'Manual',
  gated: 'Gated',
  continuous: 'Continuous',
};

/**
 * Assistant panel: sibling of TranscriptPanel in the main flex row.
 * Collapsible to a slim rail; renders sanely with the backend absent
 * (AssistantContext defaults status to enabled=false, claudeOk=true).
 */
export function AssistantPanel() {
  const router = useRouter();
  const {
    status,
    cards,
    note,
    setEnabled,
    setListening,
    setMode,
    explain,
    catchup,
    setBrief,
  } = useAssistant();

  const [collapsed, setCollapsed] = useState(false);
  const [briefValue, setBriefValue] = useState('');

  if (collapsed) {
    return (
      <div className="flex w-10 flex-col items-center gap-3 border-l border-gray-200 bg-white py-3">
        <Button
          variant="ghost"
          size="icon"
          onClick={() => setCollapsed(false)}
          title="Expand assistant panel"
        >
          <ChevronsLeft className="h-4 w-4" />
        </Button>
        <Sparkles className={cnStatus(status.enabled)} />
      </div>
    );
  }

  const showBrief = !status.sessionOpen && cards.length === 0 && note.state === 'idle';
  const actionsDisabled = !status.enabled || !status.claudeOk || !status.sessionOpen;

  const cycleMode = () => {
    const idx = MODE_ORDER.indexOf(status.mode);
    const next = MODE_ORDER[(idx + 1) % MODE_ORDER.length];
    setMode(next);
  };

  return (
    <div className="flex w-[380px] shrink-0 flex-col border-l border-gray-200 bg-white">
      {/* Header */}
      <div className="flex items-center justify-between gap-2 border-b border-gray-200 p-3">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => setListening(!status.listening)}
            title={status.listening ? 'Pause triggers' : 'Resume triggers'}
            className="flex items-center justify-center"
          >
            <span
              className={cnDot(status.enabled && status.listening)}
            />
          </button>
          <button
            type="button"
            onClick={cycleMode}
            title="Cycle trigger mode"
            className="rounded-full border border-gray-200 bg-gray-50 px-2 py-0.5 text-xs font-medium text-gray-600 hover:bg-gray-100"
          >
            {MODE_LABELS[status.mode] ?? status.mode}
          </button>
        </div>
        <div className="flex items-center gap-2">
          <Switch checked={status.enabled} onCheckedChange={setEnabled} />
          <Button variant="ghost" size="icon" onClick={() => setCollapsed(true)} title="Collapse">
            <ChevronsRight className="h-4 w-4" />
          </Button>
        </div>
      </div>

      {/* Optional meeting brief, before a session opens */}
      {showBrief && (
        <div className="border-b border-gray-200 p-3">
          <label className="mb-1 block text-xs font-medium text-gray-500">
            Meeting brief (optional)
          </label>
          <Textarea
            value={briefValue}
            onChange={(e) => setBriefValue(e.target.value)}
            onBlur={() => setBrief(briefValue)}
            placeholder="What is this meeting about?"
            className="min-h-[60px] text-sm"
          />
        </div>
      )}

      {/* Body: empty states or card stack */}
      <div className="flex flex-1 flex-col overflow-hidden">
        {!status.enabled ? (
          <EmptyState
            icon={<Sparkles className="h-5 w-5 text-gray-400" />}
            message="Assistant is off."
            action={<Button size="sm" onClick={() => setEnabled(true)}>Enable</Button>}
          />
        ) : !status.claudeOk ? (
          <EmptyState
            icon={<AlertCircle className="h-5 w-5 text-amber-500" />}
            message="Claude CLI isn't available."
            action={
              <Button size="sm" variant="outline" onClick={() => router.push('/settings')}>
                Open Settings
              </Button>
            }
          />
        ) : status.sessionOpen && !status.lanesReady ? (
          <div className="flex flex-1 flex-col gap-2 p-3">
            <div className="h-16 animate-pulse rounded-lg bg-gray-100" />
            <div className="h-16 animate-pulse rounded-lg bg-gray-100" />
            <p className="text-center text-xs text-gray-400">Lanes warming up...</p>
          </div>
        ) : cards.length === 0 ? (
          <EmptyState
            icon={<Sparkles className="h-5 w-5 text-gray-400" />}
            message="No cards yet."
            hint="Option-A voice ask, Option-E explain, Option-C catch up, Option-M cycle mode"
          />
        ) : (
          <ScrollArea className="flex-1">
            <div className="flex flex-col gap-2 p-3">
              {cards.map((card) => (
                <AssistantCard key={card.id} card={card} />
              ))}
            </div>
          </ScrollArea>
        )}
      </div>

      {(!status.sessionOpen && cards.length > 0) || note.state !== 'idle' ? <NoteBar /> : null}

      <div className="flex gap-2 border-t border-gray-200 p-3">
        <Button
          variant="outline"
          size="sm"
          className="flex-1"
          onClick={explain}
          disabled={actionsDisabled}
        >
          <MessageCircleQuestion className="h-4 w-4" />
          Explain
        </Button>
        <Button
          variant="outline"
          size="sm"
          className="flex-1"
          onClick={catchup}
          disabled={actionsDisabled}
        >
          <History className="h-4 w-4" />
          Catch up
        </Button>
      </div>

      <AskBox disabled={actionsDisabled} />
    </div>
  );
}

function EmptyState({
  icon,
  message,
  hint,
  action,
}: {
  icon: React.ReactNode;
  message: string;
  hint?: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-2 p-6 text-center">
      {icon}
      <p className="text-sm text-gray-500">{message}</p>
      {hint && <p className="text-xs text-gray-400">{hint}</p>}
      {action}
    </div>
  );
}

function cnDot(active: boolean): string {
  return active
    ? 'h-2.5 w-2.5 rounded-full bg-green-500 animate-pulse'
    : 'h-2.5 w-2.5 rounded-full bg-gray-300';
}

function cnStatus(enabled: boolean): string {
  return enabled ? 'h-4 w-4 text-blue-500' : 'h-4 w-4 text-gray-300';
}
