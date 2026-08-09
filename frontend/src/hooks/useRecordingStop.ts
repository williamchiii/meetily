import { useState, useEffect, useCallback, useRef } from 'react';
import { useRouter } from 'next/navigation';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { useTranscripts } from '@/contexts/TranscriptContext';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { useRecordingState, RecordingStatus } from '@/contexts/RecordingStateContext';
import { storageService } from '@/services/storageService';
import { transcriptService } from '@/services/transcriptService';
import { useResumeRecording } from '@/hooks/useResumeRecording';
import { takeActiveResumeMeetingId } from '@/lib/resume-intent';
import { useScreenshots } from '@/contexts/ScreenshotContext';
import Analytics from '@/lib/analytics';
import {
  applyPinnedSummaryLanguageToMeeting,
  detectAndCacheSummaryLanguage,
} from '@/lib/summary-language-preferences';

type SummaryStatus = 'idle' | 'processing' | 'summarizing' | 'regenerating' | 'completed' | 'error';

interface UseRecordingStopReturn {
  handleRecordingStop: (callApi: boolean) => Promise<void>;
  isStopping: boolean;
  isProcessingTranscript: boolean;
  isSavingTranscript: boolean;
  summaryStatus: SummaryStatus;
  setIsStopping: (value: boolean) => void;
}

/**
 * Custom hook for managing recording stop lifecycle.
 * Handles the complex stop sequence: transcription wait → buffer flush → SQLite save → navigation.
 *
 * Features:
 * - Transcription completion polling (60s max, 500ms interval)
 * - Transcript buffer flush coordination
 * - SQLite meeting save with folder_path from sessionStorage
 * - Comprehensive analytics tracking (duration, word count, activation)
 * - Auto-navigation to meeting details
 * - Toast notifications for success/error
 * - Window exposure for Rust callbacks
 */
export function useRecordingStop(
  setIsRecording: (value: boolean) => void,
  setIsRecordingDisabled: (value: boolean) => void
): UseRecordingStopReturn {
  // USE global state instead
  const recordingState = useRecordingState();
  const {
    status,
    setStatus,
    isStopping,
    isProcessing: isProcessingTranscript,
    isSaving: isSavingTranscript
  } = recordingState;

  const {
    transcriptsRef,
    flushBuffer,
    clearTranscripts,
    meetingTitle,
    markMeetingAsSaved,
  } = useTranscripts();

  const {
    refetchMeetings,
    setCurrentMeeting,
    setMeetings,
    meetings,
    setIsMeetingActive,
  } = useSidebar();

  const router = useRouter();
  const resumeRecording = useResumeRecording();
  const { attachToMeeting: attachScreenshots } = useScreenshots();

  // Guard to prevent duplicate/concurrent stop calls (e.g., from UI and tray simultaneously)
  const stopInProgressRef = useRef(false);

  // Whether this stop was the call detector's doing rather than the user's
  const autoStoppedRef = useRef(false);

  // Set when the user resumes from the auto-stop toast, so the post-stop
  // navigation does not drag them off the recording page they just returned to
  const resumeRequestedRef = useRef(false);

  // Track unattended stops so they can offer a way back into the same meeting
  useEffect(() => {
    let unlistenAutoStop: (() => void) | undefined;
    let unlistenStarted: (() => void) | undefined;

    const setupListeners = async () => {
      try {
        unlistenAutoStop = await listen('recording-auto-stopped', () => {
          autoStoppedRef.current = true;
        });
        // A fresh recording is never the tail of an earlier auto-stop
        unlistenStarted = await listen('recording-started', () => {
          autoStoppedRef.current = false;
        });
      } catch (error) {
        console.error('Failed to setup auto-stop listeners:', error);
      }
    };

    setupListeners();

    return () => {
      unlistenAutoStop?.();
      unlistenStarted?.();
    };
  }, []);

  /** Resume from the auto-stop toast, continuing the meeting that just saved. */
  const resumeFromToast = useCallback((meetingId: string, title: string) => {
    // Suppress the post-stop navigation below; resuming already moved the user
    resumeRequestedRef.current = true;
    resumeRecording(meetingId, title, 'auto_stop_toast');
  }, [resumeRecording]);

  // Promise to track recording-stopped event data (fixes race condition with recording-stop-complete)
  const recordingStoppedDataRef = useRef<Promise<void> | null>(null);

  // Set up recording-stopped listener for meeting navigation
  useEffect(() => {
    let unlistenFn: (() => void) | undefined;

    const setupRecordingStoppedListener = async () => {
      try {
        console.log('Setting up recording-stopped listener for navigation...');
        unlistenFn = await listen<{
          message: string;
          folder_path?: string;
          meeting_name?: string;
        }>('recording-stopped', async (event) => {
          // Create promise that resolves when sessionStorage is set (prevents race condition)
          recordingStoppedDataRef.current = (async () => {
            const { folder_path, meeting_name } = event.payload;

            // Store folder_path and meeting_name for later use in handleRecordingStop
            if (folder_path) {
              sessionStorage.setItem('last_recording_folder_path', folder_path);
            }
            if (meeting_name) {
              sessionStorage.setItem('last_recording_meeting_name', meeting_name);
            }
          })();

        });
        console.log('Recording stopped listener setup complete');
      } catch (error) {
        console.error('Failed to setup recording stopped listener:', error);
      }
    };

    setupRecordingStoppedListener();

    return () => {
      console.log('Cleaning up recording stopped listener...');
      if (unlistenFn) {
        unlistenFn();
      }
    };
  }, [router]);

  // Main recording stop handler
  const handleRecordingStop = useCallback(async (isCallApi: boolean) => {
    if (recordingStoppedDataRef.current) {
      await recordingStoppedDataRef.current;
    }

    // Guard: prevent duplicate/concurrent stop calls
    if (stopInProgressRef.current) {
      return;
    }
    stopInProgressRef.current = true;
    resumeRequestedRef.current = false;

    // Set status to STOPPING immediately
    setStatus(RecordingStatus.STOPPING);
    setIsRecording(false);
    setIsRecordingDisabled(true);
    const stopStartTime = Date.now();

    try {
      console.log('Post-stop processing (new implementation)...', {
        stop_initiated_at: new Date(stopStartTime).toISOString(),
        current_transcript_count: transcriptsRef.current.length
      });

      // Note: stop_recording is already called by RecordingControls.stopRecordingAction
      // This function only handles post-stop processing (transcription wait, API call, navigation)
      console.log('Recording already stopped by RecordingControls, processing transcription...');

      // Wait for transcription to complete
      setStatus(RecordingStatus.PROCESSING_TRANSCRIPTS, 'Waiting for transcription...');
      console.log('Waiting for transcription to complete...');

      const MAX_WAIT_TIME = 60000; // 60 seconds maximum wait (increased for longer processing)
      const POLL_INTERVAL = 500; // Check every 500ms
      let elapsedTime = 0;
      let transcriptionComplete = false;

      // Listen for transcription-complete event
      const unlistenComplete = await listen('transcription-complete', () => {
        console.log('Received transcription-complete event');
        transcriptionComplete = true;
      });

      // Poll for transcription status
      while (elapsedTime < MAX_WAIT_TIME && !transcriptionComplete) {
        try {
          const status = await transcriptService.getTranscriptionStatus();
          console.log('Transcription status:', status);

          // Check if transcription is complete
          if (!status.is_processing && status.chunks_in_queue === 0) {
            console.log('Transcription complete - no active processing and no chunks in queue');
            transcriptionComplete = true;
            break;
          }

          // If no activity for more than 8 seconds and no chunks in queue, consider it done (increased from 5s to 8s)
          if (status.last_activity_ms > 8000 && status.chunks_in_queue === 0) {
            console.log('Transcription likely complete - no recent activity and empty queue');
            transcriptionComplete = true;
            break;
          }

          // Update user with current status
          if (status.chunks_in_queue > 0) {
            console.log(`Processing ${status.chunks_in_queue} remaining audio chunks...`);
            setStatus(RecordingStatus.PROCESSING_TRANSCRIPTS, `Processing ${status.chunks_in_queue} remaining chunks...`);
          }

          // Wait before next check
          await new Promise(resolve => setTimeout(resolve, POLL_INTERVAL));
          elapsedTime += POLL_INTERVAL;
        } catch (error) {
          console.error('Error checking transcription status:', error);
          break;
        }
      }

      // Clean up listener
      console.log('🧹 CLEANUP: Cleaning up transcription-complete listener');
      unlistenComplete();

      if (!transcriptionComplete && elapsedTime >= MAX_WAIT_TIME) {
        console.warn('⏰ Transcription wait timeout reached after', elapsedTime, 'ms');
      } else {
        console.log('✅ Transcription completed after', elapsedTime, 'ms');
        // Wait longer for any late transcript segments (increased from 1s to 4s)
        console.log('⏳ Waiting for late transcript segments...');
        await new Promise(resolve => setTimeout(resolve, 4000));
      }

      // Final buffer flush: process ALL remaining transcripts regardless of timing
      const flushStartTime = Date.now();
      console.log('🔄 Final buffer flush: forcing processing of any remaining transcripts...', {
        flush_started_at: new Date(flushStartTime).toISOString(),
        time_since_stop: flushStartTime - stopStartTime,
        current_transcript_count: transcriptsRef.current.length
      });
      setStatus(RecordingStatus.PROCESSING_TRANSCRIPTS, 'Flushing transcript buffer...');
      flushBuffer();
      const flushEndTime = Date.now();
      console.log('✅ Final buffer flush completed', {
        flush_duration: flushEndTime - flushStartTime,
        total_time_since_stop: flushEndTime - stopStartTime,
        final_transcript_count: transcriptsRef.current.length
      });

      // NOTE: Status remains PROCESSING_TRANSCRIPTS until we start saving

      // Wait a bit more to ensure all transcript state updates have been processed
      console.log('Waiting for transcript state updates to complete...');
      await new Promise(resolve => setTimeout(resolve, 500));

      // A resumed recording continues a meeting that is already in the database, so
      // its segments are appended instead of creating a second meeting.
      //
      // Consumed OUTSIDE the save guard below: this recording is over either way, and
      // leaving the intent behind on a failed stop would silently append the user's
      // NEXT, unrelated recording to this meeting.
      const resumingMeetingId = takeActiveResumeMeetingId(sessionStorage);

      // Save to SQLite
      // NOTE: enabled to save COMPLETE transcripts after frontend receives all updates
      // This ensures user sees all transcripts streaming in before database save
      if (isCallApi && transcriptionComplete == true) {

        setStatus(RecordingStatus.SAVING, 'Saving meeting to database...');

        // Get fresh transcript state (ALL transcripts including late ones)
        const freshTranscripts = [...transcriptsRef.current];

        // Get folder_path and meeting_name from recording-stopped event
        const folderPath = sessionStorage.getItem('last_recording_folder_path');
        const savedMeetingName = sessionStorage.getItem('last_recording_meeting_name');

        console.log('💾 Saving COMPLETE transcripts to database...', {
          transcript_count: freshTranscripts.length,
          meeting_name: savedMeetingName || meetingTitle,
          folder_path: folderPath,
          appending_to: resumingMeetingId ?? 'new meeting',
          sample_text: freshTranscripts.length > 0 ? freshTranscripts[0].text.substring(0, 50) + '...' : 'none',
          last_transcript: freshTranscripts.length > 0 ? freshTranscripts[freshTranscripts.length - 1].text.substring(0, 30) + '...' : 'none',
        });

        const saveAsNewMeeting = () => storageService.saveMeeting(
          savedMeetingName || meetingTitle || 'New Meeting',  // PREFER savedMeetingName (backend source)
          freshTranscripts,
          folderPath
        );

        try {
          let responseData;
          let appendFellBack = false;

          if (resumingMeetingId) {
            try {
              responseData = await storageService.appendToMeeting(resumingMeetingId, freshTranscripts);
            } catch (appendError) {
              // The meeting may have been deleted while this recording ran. Losing
              // the transcripts is far worse than an extra meeting, so fall back to
              // saving them on their own rather than letting the stop fail.
              console.error('Append failed, saving as a new meeting instead:', appendError);
              appendFellBack = true;
              responseData = await saveAsNewMeeting();
              toast.warning('Could not add to the original meeting', {
                description: 'The recording was saved as a separate meeting instead.',
              });
            }
          } else {
            responseData = await saveAsNewMeeting();
          }

          // A fallback save produced a brand new meeting, so nothing is being merged
          // into an existing one any more
          const appendedToMeetingId = appendFellBack ? null : resumingMeetingId;

          const meetingId = responseData.meeting_id;
          if (!meetingId) {
            console.error('No meeting_id in response:', responseData);
            throw new Error('No meeting ID received from save operation');
          }

          // Screenshots shared during the recording belong to this meeting now.
          // Best-effort: the transcript is already saved, and losing screen context
          // must not fail the stop.
          try {
            const attached = await attachScreenshots(meetingId);
            if (attached > 0) {
              console.log(`🖼️ Attached ${attached} screenshot(s) to meeting ${meetingId}`);
            }
          } catch (screenshotError) {
            console.error('Failed to attach screenshots:', screenshotError);
            toast.warning('Could not save the screenshot context', {
              description: 'The meeting was saved, but the screen context was not attached.',
            });
          }

          // Fold the resumed recording's audio into the meeting's own folder, so
          // playback and retranscription cover the whole conversation. Best-effort:
          // the transcripts are already saved, and a failed merge must not fail the
          // stop or lose the recording that is still on disk.
          if (appendedToMeetingId && folderPath) {
            try {
              const merge = await storageService.mergeResumedRecording(
                appendedToMeetingId,
                folderPath
              );

              // Every "nothing to do" path resolves successfully with merged:false
              // rather than throwing, so the flag is the only way to tell a real
              // merge from a silent no-op
              if (merge.merged) {
                console.log('🎧 Merged recording folders:', merge.detail, merge.retired_to ?? '');
              } else {
                console.warn('🎧 Recording folders NOT merged:', merge.detail);
                toast.warning('Recording folders were not merged', {
                  description: merge.detail,
                });
              }
            } catch (mergeError) {
              console.error('Failed to merge resumed recording folders:', mergeError);
              toast.warning('Could not merge the resumed recording', {
                description: 'The transcript was saved. The resumed files are still in their own folder.',
              });
            }
          }

          // The summary language was already settled when the meeting was first
          // saved, so an append leaves it alone
          if (!appendedToMeetingId) {
            let shouldDetectSummaryLanguage = false;
            try {
              shouldDetectSummaryLanguage = !(await applyPinnedSummaryLanguageToMeeting(meetingId));
            } catch (error) {
              console.warn('Failed to apply pinned summary language preference for new meeting:', error);
              toast.warning('Could not apply default summary language', {
                description: 'The meeting was saved, but the default summary language was not applied.',
              });
            }

            if (shouldDetectSummaryLanguage) {
              try {
                await detectAndCacheSummaryLanguage(
                  meetingId,
                  freshTranscripts.map(t => t.text)
                );
              } catch (error) {
                console.warn('Failed to detect summary language for new meeting:', error);
                toast.warning('Could not detect summary language', {
                  description: 'The meeting was saved, but Auto could not detect the summary language.',
                });
              }
            }
          }

          console.log('✅ Successfully saved COMPLETE meeting with ID:', meetingId);
          console.log('   Transcripts:', freshTranscripts.length);
          console.log('   folder_path:', folderPath);

          // Mark meeting as saved in IndexedDB (for recovery system)
          await markMeetingAsSaved();

          // Clean up session storage
          sessionStorage.removeItem('last_recording_folder_path');
          sessionStorage.removeItem('last_recording_meeting_name');
          // Clean up IndexedDB meeting ID (redundant with markMeetingAsSaved cleanup, but ensures cleanup)
          sessionStorage.removeItem('indexeddb_current_meeting_id');

          // Refetch meetings and set current meeting
          await refetchMeetings();

          try {
            const meetingData = await storageService.getMeeting(meetingId);
            if (meetingData) {
              setCurrentMeeting({
                id: meetingId,
                title: meetingData.title
              });
              console.log('✅ Current meeting set:', meetingData.title);
            }
          } catch (error) {
            console.warn('Could not fetch meeting details, using ID only:', error);
            setCurrentMeeting({ id: meetingId, title: savedMeetingName || meetingTitle || 'New Meeting' });
          }

          // Mark as completed
          setStatus(RecordingStatus.COMPLETED);

          const wasAutoStopped = autoStoppedRef.current;
          autoStoppedRef.current = false;

          if (wasAutoStopped) {
            // The user never asked for this stop, so offer the way back: resuming
            // continues this same meeting rather than starting a second one.
            toast.success('Recording stopped - call ended', {
              description: `${freshTranscripts.length} transcript segments saved. Still talking?`,
              action: {
                label: 'Resume recording',
                onClick: () => resumeFromToast(
                  meetingId,
                  savedMeetingName || meetingTitle || 'New Meeting'
                )
              },
              duration: 60000,
            });
          } else {
            // Show success toast with navigation option
            toast.success('Recording saved successfully!', {
              description: `${freshTranscripts.length} transcript segments saved.`,
              action: {
                label: 'View Meeting',
                onClick: () => {
                  router.push(`/meeting-details?id=${meetingId}`);
                  Analytics.trackButtonClick('view_meeting_from_toast', 'recording_complete');
                }
              },
              duration: 10000,
            });
          }

          // Auto-navigate after a short delay with source parameter
          setTimeout(() => {
            // Resuming already sent the user back to the recording page, and a new
            // recording may already be under way - clearing/navigating now would
            // clobber its transcripts and status instead of this finished meeting's
            if (resumeRequestedRef.current) {
              return;
            }

            // This meeting is finished, so release its transcripts and status
            // before deciding where to send the user - skipping this would leave
            // the finished meeting's segments in the shared list and the status
            // pinned at COMPLETED
            clearTranscripts();
            setStatus(RecordingStatus.IDLE);

            // `merged` tells the meeting page the summary is now stale: it covers
            // only the first stretch of a conversation that just grew
            const mergedParam = appendedToMeetingId ? '&merged=true' : '';
            router.push(`/meeting-details?id=${meetingId}&source=recording${mergedParam}`);
            Analytics.trackPageView('meeting_details');
          }, 2000);
          // Track meeting completion analytics
          try {
            // Calculate meeting duration from transcript timestamps
            let durationSeconds = 0;
            if (freshTranscripts.length > 0 && freshTranscripts[0].audio_start_time !== undefined) {
              // Use audio_end_time of last transcript if available
              const lastTranscript = freshTranscripts[freshTranscripts.length - 1];
              durationSeconds = lastTranscript.audio_end_time || lastTranscript.audio_start_time || 0;
            }

            // Calculate word count
            const transcriptWordCount = freshTranscripts
              .map(t => t.text.split(/\s+/).length)
              .reduce((a, b) => a + b, 0);

            // Calculate words per minute
            const wordsPerMinute = durationSeconds > 0 ? transcriptWordCount / (durationSeconds / 60) : 0;

            // Get meetings count today
            const meetingsToday = await Analytics.getMeetingsCountToday();

            // Track meeting completed
            await Analytics.trackMeetingCompleted(meetingId, {
              duration_seconds: durationSeconds,
              transcript_segments: freshTranscripts.length,
              transcript_word_count: transcriptWordCount,
              words_per_minute: wordsPerMinute,
              meetings_today: meetingsToday
            });

            // Update meeting count in analytics.json
            await Analytics.updateMeetingCount();

            // Check for activation (first meeting)
            const { Store } = await import('@tauri-apps/plugin-store');
            const store = await Store.load('analytics.json');
            const totalMeetings = await store.get<number>('total_meetings');

            if (totalMeetings === 1) {
              const daysSinceInstall = await Analytics.calculateDaysSince('first_launch_date');
              await Analytics.track('user_activated', {
                meetings_count: '1',
                days_since_install: daysSinceInstall?.toString() || 'null',
                first_meeting_duration_seconds: durationSeconds.toString()
              });
            }
          } catch (analyticsError) {
            console.error('Failed to track meeting completion analytics:', analyticsError);
            // Don't block user flow on analytics errors
          }

        } catch (saveError) {
          console.error('Failed to save meeting to database:', saveError);
          setStatus(RecordingStatus.ERROR, saveError instanceof Error ? saveError.message : 'Unknown error');
          toast.error('Failed to save meeting', {
            description: saveError instanceof Error ? saveError.message : 'Unknown error'
          });
          throw saveError;
        }
      } else {
        // No save needed, go back to IDLE
        setStatus(RecordingStatus.IDLE);
      }

      setIsMeetingActive(false);
      // isRecording already set to false at function start
      setIsRecordingDisabled(false);
    } catch (error) {
      console.error('Error in handleRecordingStop:', error);
      setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Unknown error');
      // isRecording already set to false at function start
      setIsRecordingDisabled(false);
    } finally {
      // Always reset the guard flag when done
      stopInProgressRef.current = false;
    }
  }, [
    setIsRecording,
    setIsRecordingDisabled,
    setStatus,
    transcriptsRef,
    flushBuffer,
    clearTranscripts,
    meetingTitle,
    markMeetingAsSaved,
    refetchMeetings,
    setCurrentMeeting,
    setMeetings,
    meetings,
    setIsMeetingActive,
    router,
    resumeFromToast,
    attachScreenshots,
  ]);

  // Expose handleRecordingStop function to window for Rust callbacks
  const handleRecordingStopRef = useRef(handleRecordingStop);
  useEffect(() => {
    handleRecordingStopRef.current = handleRecordingStop;
  });

  useEffect(() => {
    (window as any).handleRecordingStop = (callApi: boolean = true) => {
      handleRecordingStopRef.current(callApi);
    };

    // Cleanup on unmount
    return () => {
      delete (window as any).handleRecordingStop;
    };
  }, []);

  // Derive summaryStatus from RecordingStatus for backward compatibility
  const summaryStatus: SummaryStatus = status === RecordingStatus.PROCESSING_TRANSCRIPTS ? 'processing' : 'idle';

  return {
    handleRecordingStop,
    isStopping,
    isProcessingTranscript,
    isSavingTranscript,
    summaryStatus,
    setIsStopping: (value: boolean) => {
      setStatus(value ? RecordingStatus.STOPPING : RecordingStatus.IDLE);
    },
  };
}
