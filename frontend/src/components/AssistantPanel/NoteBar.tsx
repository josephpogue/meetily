'use client';

import ReactMarkdown, { Components } from 'react-markdown';
import { Check, FileText, Loader2, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useAssistant } from '@/contexts/AssistantContext';

// Styled explicitly (not via the Tailwind typography plugin, whose availability
// is ambiguous here: the repo carries both tailwind.config.js and .ts, and only
// the .ts one lists the plugin).
const markdownComponents: Components = {
  h1: ({ children }) => <h1 className="mb-1 mt-3 text-base font-semibold text-gray-900 first:mt-0">{children}</h1>,
  h2: ({ children }) => <h2 className="mb-1 mt-3 text-sm font-semibold text-gray-900 first:mt-0">{children}</h2>,
  h3: ({ children }) => <h3 className="mb-1 mt-2 text-sm font-semibold text-gray-800">{children}</h3>,
  p: ({ children }) => <p className="mb-2 text-sm text-gray-700">{children}</p>,
  ul: ({ children }) => <ul className="mb-2 ml-4 list-disc space-y-0.5 text-sm text-gray-700">{children}</ul>,
  ol: ({ children }) => <ol className="mb-2 ml-4 list-decimal space-y-0.5 text-sm text-gray-700">{children}</ol>,
  li: ({ children }) => <li>{children}</li>,
  strong: ({ children }) => <strong className="font-semibold text-gray-900">{children}</strong>,
  a: ({ children, href }) => (
    <a href={href} className="text-blue-600 underline" target="_blank" rel="noreferrer">
      {children}
    </a>
  ),
};

/**
 * End-of-meeting note flow: Draft note -> preview -> Save / Discard.
 * Save is the only write; nothing else in the panel writes anywhere.
 */
export function NoteBar() {
  const { note, draftNote, saveNote, discardNote } = useAssistant();

  return (
    <div className="border-t border-gray-200 bg-gray-50 p-3">
      {note.state === 'idle' && (
        <Button variant="outline" size="sm" className="w-full" onClick={draftNote}>
          <FileText className="h-4 w-4" />
          Draft note
        </Button>
      )}

      {note.state === 'drafting' && (
        <div className="flex items-center justify-center gap-2 py-2 text-sm text-gray-500">
          <Loader2 className="h-4 w-4 animate-spin" />
          Drafting note...
        </div>
      )}

      {note.state === 'ready' && (
        <div className="space-y-2">
          <ScrollArea className="h-48 rounded-md border border-gray-200 bg-white p-3">
            <ReactMarkdown components={markdownComponents}>{note.markdown}</ReactMarkdown>
          </ScrollArea>
          <div className="flex gap-2">
            <Button variant="blue" size="sm" className="flex-1" onClick={saveNote}>
              <Check className="h-4 w-4" />
              Save
            </Button>
            <Button variant="outline" size="sm" className="flex-1" onClick={discardNote}>
              <X className="h-4 w-4" />
              Discard
            </Button>
          </div>
        </div>
      )}

      {note.state === 'saved' && <p className="text-center text-sm text-green-600">Note saved.</p>}

      {note.state === 'failed' && (
        <div className="space-y-2">
          <p className="text-sm text-red-600">{note.error || 'Note drafting failed.'}</p>
          <Button variant="outline" size="sm" className="w-full" onClick={draftNote}>
            Try again
          </Button>
        </div>
      )}
    </div>
  );
}
