'use client';

import { Mic } from 'lucide-react';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { useResumeRecording } from '@/hooks/useResumeRecording';

interface ResumeRecordingButtonProps {
  meetingId: string;
  meetingTitle: string;
}

/**
 * Picks a meeting back up: starts a new recording whose transcripts are appended to
 * this meeting rather than saved as a separate one.
 *
 * Useful after the call detector ends a recording early, and for any meeting that
 * carried on after recording stopped.
 */
export function ResumeRecordingButton({ meetingId, meetingTitle }: ResumeRecordingButtonProps) {
  const { isRecording } = useRecordingState();
  const resumeRecording = useResumeRecording();

  if (!meetingId) {
    return null;
  }

  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <button
            onClick={() => resumeRecording(meetingId, meetingTitle, 'meeting_details')}
            disabled={isRecording}
            className={`flex items-center gap-1.5 px-2.5 py-1.5 rounded-md text-sm border transition-colors flex-shrink-0 ${
              isRecording
                ? 'border-gray-200 text-gray-300 cursor-not-allowed'
                : 'border-gray-300 text-gray-600 hover:text-red-600 hover:border-red-300 hover:bg-red-50'
            }`}
            aria-label="Resume recording into this meeting"
          >
            <Mic className="w-4 h-4" />
            <span className="hidden sm:inline">Resume</span>
          </button>
        </TooltipTrigger>
        <TooltipContent>
          <p>
            {isRecording
              ? 'A recording is already in progress'
              : 'Record more into this meeting'}
          </p>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
