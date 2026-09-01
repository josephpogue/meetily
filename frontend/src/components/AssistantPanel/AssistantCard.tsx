'use client';

import { useEffect, useState } from 'react';
import { AssistantCard as AssistantCardData } from '@/types';
import { cn } from '@/lib/utils';

interface AssistantCardProps {
  card: AssistantCardData;
}

const KIND_LABELS: Record<AssistantCardData['kind'], string> = {
  answer: 'Answer',
  ask: 'Ask',
  explain: 'Explain',
  catchup: 'Catch up',
};

function formatTime(ts: number): string {
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return '';
  return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

/**
 * A single assistant card: lead line, up to 3 bullets, small mono source line.
 * Phase styling: drafting streams in with an amber accent, checked is neutral,
 * corrected carries a badge and briefly highlights the bullets that changed.
 */
export function AssistantCard({ card }: AssistantCardProps) {
  const isDrafting = card.phase === 'drafting';
  const isCorrected = card.phase === 'corrected';

  const [highlightChanges, setHighlightChanges] = useState(isCorrected && card.changedLines.length > 0);

  useEffect(() => {
    if (isCorrected && card.changedLines.length > 0) {
      setHighlightChanges(true);
      const timer = setTimeout(() => setHighlightChanges(false), 2500);
      return () => clearTimeout(timer);
    }
    setHighlightChanges(false);
  }, [card.id, card.phase, card.changedLines]);

  return (
    <div
      className={cn(
        'rounded-lg border bg-white p-3 shadow-sm',
        isDrafting
          ? 'border-y-gray-200 border-r-gray-200 border-l-4 border-l-amber-400 bg-amber-50/40'
          : 'border-gray-200'
      )}
    >
      <div className="mb-1.5 flex items-center justify-between gap-2">
        <div className="flex items-center gap-1.5">
          <span className="text-[10px] font-medium uppercase tracking-wide text-gray-400">
            {KIND_LABELS[card.kind]}
          </span>
          {isDrafting && <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400" />}
          {isCorrected && (
            <span className="rounded-full border border-blue-200 bg-blue-50 px-1.5 py-0.5 text-[10px] font-medium text-blue-700">
              Corrected
            </span>
          )}
        </div>
        <span className="text-[11px] text-gray-400">{formatTime(card.ts)}</span>
      </div>

      {card.question && (
        <p className="mb-1 text-xs italic text-gray-500">&ldquo;{card.question}&rdquo;</p>
      )}

      {card.lead && <p className="text-sm font-semibold text-gray-900">{card.lead}</p>}

      {card.bullets.length > 0 && (
        <ul className="mt-1.5 space-y-1">
          {card.bullets.slice(0, 3).map((bullet, i) => (
            <li
              key={i}
              className={cn(
                'relative rounded pl-3 text-sm text-gray-700 transition-colors duration-1000 before:absolute before:left-0 before:text-gray-400 before:content-["-"]',
                highlightChanges && card.changedLines.includes(bullet) && 'bg-blue-50'
              )}
            >
              {bullet}
            </li>
          ))}
        </ul>
      )}

      {card.source && <p className="mt-2 font-mono text-[11px] text-gray-400">{card.source}</p>}
    </div>
  );
}
