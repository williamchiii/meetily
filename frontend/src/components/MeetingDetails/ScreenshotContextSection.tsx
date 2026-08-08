'use client';

import React, { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { ChevronDown, ChevronRight, Image as ImageIcon } from 'lucide-react';

export interface MeetingScreenshot {
  id: string;
  meeting_id: string;
  image_path: string | null;
  extracted_text: string;
  note: string | null;
  captured_at: string;
}

/**
 * What the AI read off screenshots shared during the meeting, shown above the first
 * transcript line.
 *
 * This is meeting content the microphone never captured, so it belongs in the
 * transcript view rather than buried in the summary - and seeing it here is how a
 * user checks what the summary was actually told.
 */
export function ScreenshotContextSection({ meetingId }: { meetingId?: string }) {
  const [screenshots, setScreenshots] = useState<MeetingScreenshot[]>([]);
  const [isExpanded, setIsExpanded] = useState(false);

  useEffect(() => {
    if (!meetingId) return;

    let cancelled = false;

    const load = async () => {
      try {
        const rows = await invoke<MeetingScreenshot[]>('api_get_meeting_screenshots', { meetingId });
        if (!cancelled) setScreenshots(rows);
      } catch (error) {
        // A meeting with no screen context is the common case, not a problem worth
        // showing the user
        console.warn('Could not load screenshot context:', error);
      }
    };

    load();
    return () => {
      cancelled = true;
    };
  }, [meetingId]);

  if (screenshots.length === 0) return null;

  return (
    <div className="mx-3 mt-3 mb-1 rounded-md border border-gray-200 bg-gray-50">
      <button
        onClick={() => setIsExpanded(v => !v)}
        className="w-full flex items-center gap-1.5 px-3 py-2 text-left"
        aria-expanded={isExpanded}
      >
        {isExpanded ? (
          <ChevronDown className="w-3.5 h-3.5 text-gray-500 flex-shrink-0" />
        ) : (
          <ChevronRight className="w-3.5 h-3.5 text-gray-500 flex-shrink-0" />
        )}
        <ImageIcon className="w-3.5 h-3.5 text-gray-500 flex-shrink-0" />
        <span className="text-xs font-semibold text-gray-800">[Screenshot context]</span>
        <span className="text-xs text-gray-500 ml-auto">
          {screenshots.length} {screenshots.length === 1 ? 'screen' : 'screens'}
        </span>
      </button>

      {isExpanded && (
        <div className="px-3 pb-3 space-y-3">
          <p className="text-[11px] text-gray-500 leading-snug">
            Shared on screen during the meeting and read by AI. Included when the summary is generated.
          </p>

          {screenshots.map((shot, index) => (
            <div key={shot.id} className="text-xs">
              <div className="font-medium text-gray-800">
                Screen {index + 1}
                {shot.note ? <span className="font-normal text-gray-600"> · {shot.note}</span> : null}
              </div>
              <pre className="mt-1 whitespace-pre-wrap font-sans text-gray-700 leading-relaxed">
                {shot.extracted_text}
              </pre>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
