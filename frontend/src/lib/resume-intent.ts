/**
 * Resume intent: the handoff that lets a new recording continue an existing meeting.
 *
 * Three moments, three different pieces of the app:
 *
 *   1. The user asks to resume (toast action, or the button on a meeting page)
 *      -> `stashResumeIntent`
 *   2. A recording actually starts             -> `claimResumeIntent`
 *   3. That recording stops and saves          -> `takeActiveResumeMeetingId`
 *
 * Step 2 is deliberately the only claim point, and it always clears the pending
 * intent whether or not it uses it. An ordinary "start recording" therefore drops a
 * leftover intent instead of silently appending to a meeting the user resumed and
 * then abandoned.
 */

/** How long a "resume this meeting" intent stays good for once the user asks for it. */
export const RESUME_INTENT_TTL_MS = 10 * 60 * 1000;

const PENDING_ID = 'pending_resume_meeting_id';
const PENDING_TITLE = 'pending_resume_meeting_title';
const PENDING_EXPIRES_AT = 'pending_resume_expires_at';
const ACTIVE_ID = 'resuming_meeting_id';

export interface ResumeIntent {
  meetingId: string;
  title: string;
}

/** The slice of the Storage API these helpers need (sessionStorage in the app). */
export interface IntentStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** Record that the next recording to start should continue `meetingId`. */
export function stashResumeIntent(
  storage: IntentStorage,
  meetingId: string,
  title: string,
  now: number = Date.now()
): void {
  storage.setItem(PENDING_ID, meetingId);
  storage.setItem(PENDING_TITLE, title);
  storage.setItem(PENDING_EXPIRES_AT, String(now + RESUME_INTENT_TTL_MS));
}

/**
 * Claim a pending intent as a recording starts, promoting it to the active one.
 *
 * Returns null when there is nothing to resume, or when the intent went stale - a
 * resume that never made it as far as starting (blocked on a missing transcription
 * model, say) must not capture some unrelated recording an hour later.
 */
export function claimResumeIntent(
  storage: IntentStorage,
  now: number = Date.now()
): ResumeIntent | null {
  const meetingId = storage.getItem(PENDING_ID);
  const title = storage.getItem(PENDING_TITLE);
  const expiresAt = Number(storage.getItem(PENDING_EXPIRES_AT));

  storage.removeItem(PENDING_ID);
  storage.removeItem(PENDING_TITLE);
  storage.removeItem(PENDING_EXPIRES_AT);

  // A missing or unparseable expiry reads as NaN, and every comparison against NaN
  // is false - so it has to be rejected outright rather than compared, or a corrupt
  // value would make the intent live forever
  if (!meetingId || !Number.isFinite(expiresAt) || now > expiresAt) {
    // Covers the ordinary case too: no intent means clear whatever was active
    storage.removeItem(ACTIVE_ID);
    return null;
  }

  storage.setItem(ACTIVE_ID, meetingId);
  return { meetingId, title: title || '' };
}

/**
 * The meeting the recording that just stopped should append to, if any.
 * Consumed here so a later recording cannot append to it a second time.
 */
export function takeActiveResumeMeetingId(storage: IntentStorage): string | null {
  const meetingId = storage.getItem(ACTIVE_ID);
  storage.removeItem(ACTIVE_ID);
  return meetingId || null;
}
