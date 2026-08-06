import { useCallback } from 'react';
import { useRouter } from 'next/navigation';
import Analytics from '@/lib/analytics';
import { stashResumeIntent } from '@/lib/resume-intent';

/**
 * Start recording again into a meeting that already exists.
 *
 * The meeting is handed to the next recording through sessionStorage:
 * `useRecordingStart` claims the intent when a recording actually starts, and
 * `useRecordingStop` appends that recording's transcripts to the meeting instead of
 * creating a second one.
 *
 * Used both by the toast shown when the call detector stops a recording and by the
 * Resume recording button on a meeting's page.
 */
export function useResumeRecording() {
  const router = useRouter();

  return useCallback((meetingId: string, title: string, source: string) => {
    stashResumeIntent(sessionStorage, meetingId, title);

    Analytics.trackButtonClick('resume_recording', source);

    // Same handoff the sidebar uses: a custom event when the recording page is
    // already mounted, the auto-start flag when we have to navigate to it first.
    //
    // The route is read at click time, NOT captured. The auto-stop toast lives for a
    // minute and its onClick closure is frozen when the toast is created - back when
    // the user was still on '/'. Two seconds later the stop handler navigates them to
    // the meeting page, so a captured pathname would fire a CustomEvent at a listener
    // that unmounted with the recording page, and nothing would happen at all.
    if (window.location.pathname === '/') {
      window.dispatchEvent(new CustomEvent('start-recording-from-sidebar'));
    } else {
      sessionStorage.setItem('autoStartRecording', 'true');
      router.push('/');
    }
  }, [router]);
}
