'use client';

import React, { useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { useRecordingStop } from '@/hooks/useRecordingStop';

/**
 * RecordingPostProcessingProvider
 *
 * This provider handles post-processing when recording stops from any source:
 * - Tray menu stop
 * - Global keyboard shortcut
 * - Overlay stop button
 * - Main UI stop button
 *
 * - Automatic stop when the meeting call ends
 *
 * It listens for the 'recording-stop-complete' event from Rust backend
 * and triggers the full post-processing flow (save to database, navigate, analytics)
 * regardless of which page the user is currently on.
 */
export function RecordingPostProcessingProvider({ children }: { children: React.ReactNode }) {
  // No-op functions since the global RecordingStateContext already handles state updates
  // These are only needed for the hook's local component state management
  const setIsRecording = () => { };
  const setIsRecordingDisabled = () => { };

  const {
    handleRecordingStop,
  } = useRecordingStop(setIsRecording, setIsRecordingDisabled);

  useEffect(() => {
    let unlistenFn: (() => void) | undefined;

    const setupListener = async () => {
      try {
        // Listen for recording-stop-complete event from Rust
        unlistenFn = await listen<boolean>('recording-stop-complete', (event) => {
          console.log('[RecordingPostProcessing] Received recording-stop-complete event:', event.payload);

          // Call the post-processing handler
          // event.payload is the callApi boolean (true for normal stops)
          handleRecordingStop(event.payload);
        });

        console.log('[RecordingPostProcessing] Event listener set up successfully');
      } catch (error) {
        console.error('[RecordingPostProcessing] Failed to set up event listener:', error);
      }
    };

    setupListener();

    return () => {
      if (unlistenFn) {
        console.log('[RecordingPostProcessing] Cleaning up event listener');
        unlistenFn();
      }
    };
  }, [handleRecordingStop]);

  // Explain unattended stops, which otherwise look like the app quit on its own
  useEffect(() => {
    let unlistenFn: (() => void) | undefined;

    const setupListener = async () => {
      try {
        unlistenFn = await listen<string[]>('recording-auto-stopped', (event) => {
          const apps = event.payload ?? [];
          console.log('[RecordingPostProcessing] Call ended, stopping recording:', apps);

          toast.info('Call ended - stopping recording', {
            description: apps.length > 0
              ? `Your ${apps.join(', ')} call ended. Saving the meeting.`
              : 'Saving the meeting.',
            duration: 8000,
          });
        });
      } catch (error) {
        console.error('[RecordingPostProcessing] Failed to set up auto-stop listener:', error);
      }
    };

    setupListener();

    return () => {
      if (unlistenFn) {
        unlistenFn();
      }
    };
  }, []);

  return <>{children}</>;
}
