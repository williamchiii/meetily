import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import ts from 'typescript';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const modulePath = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  'src',
  'lib',
  'resume-intent.ts'
);
const require = createRequire(import.meta.url);

function loadTsModule(filePath) {
  const source = fs.readFileSync(filePath, 'utf8');
  const compiled = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2020,
    },
  }).outputText;

  const module = { exports: {} };
  vm.runInNewContext(compiled, { exports: module.exports, module, require });
  return module.exports;
}

const {
  RESUME_INTENT_TTL_MS,
  stashResumeIntent,
  claimResumeIntent,
  takeActiveResumeMeetingId,
} = loadTsModule(modulePath);

/** Stand-in for sessionStorage. */
function fakeStorage(initial = {}) {
  const data = new Map(Object.entries(initial));
  return {
    getItem: (key) => (data.has(key) ? data.get(key) : null),
    setItem: (key, value) => data.set(key, String(value)),
    removeItem: (key) => data.delete(key),
    keys: () => [...data.keys()].sort(),
  };
}

/**
 * Objects come back from the vm realm with a foreign prototype, which strict
 * deep-equal rejects. Re-create them in this realm before comparing.
 */
const plain = (value) => (value === null ? null : { ...value });

const NOW = 1_770_000_000_000;

// --- The happy path: resume -> start -> stop -----------------------------------

{
  const storage = fakeStorage();

  stashResumeIntent(storage, 'meeting-abc', 'Weekly Sync', NOW);

  const intent = claimResumeIntent(storage, NOW + 5_000);
  assert.deepEqual(plain(intent), { meetingId: 'meeting-abc', title: 'Weekly Sync' });

  // The recording that just started knows which meeting it continues
  assert.equal(takeActiveResumeMeetingId(storage), 'meeting-abc');

  // ...and only once, so a later recording cannot append to it again
  assert.equal(takeActiveResumeMeetingId(storage), null);
  assert.deepEqual(storage.keys(), [], 'every key should be cleaned up');
}

// --- An ordinary start is never a resume ---------------------------------------

{
  const storage = fakeStorage();

  assert.equal(claimResumeIntent(storage, NOW), null);
  assert.equal(takeActiveResumeMeetingId(storage), null);
}

// --- A start with no intent clears a leftover active id -------------------------

{
  // A previous resume left an active meeting behind (recording died mid-flight)
  const storage = fakeStorage({ resuming_meeting_id: 'meeting-stale' });

  assert.equal(claimResumeIntent(storage, NOW), null);
  assert.equal(
    takeActiveResumeMeetingId(storage),
    null,
    'an ordinary start must not inherit an abandoned meeting'
  );
}

// --- Stale intents are dropped --------------------------------------------------

{
  const storage = fakeStorage();
  stashResumeIntent(storage, 'meeting-abc', 'Weekly Sync', NOW);

  // One millisecond past the TTL - the user resumed, got blocked, and came back later
  const intent = claimResumeIntent(storage, NOW + RESUME_INTENT_TTL_MS + 1);

  assert.equal(intent, null);
  assert.equal(takeActiveResumeMeetingId(storage), null);
  assert.deepEqual(storage.keys(), [], 'a stale intent should leave nothing behind');
}

// --- ...but one right on the boundary still counts ------------------------------

{
  const storage = fakeStorage();
  stashResumeIntent(storage, 'meeting-abc', 'Weekly Sync', NOW);

  const intent = claimResumeIntent(storage, NOW + RESUME_INTENT_TTL_MS);
  assert.deepEqual(plain(intent), { meetingId: 'meeting-abc', title: 'Weekly Sync' });
}

// --- Claiming twice: the second start is not a resume ---------------------------

{
  const storage = fakeStorage();
  stashResumeIntent(storage, 'meeting-abc', 'Weekly Sync', NOW);

  assert.ok(claimResumeIntent(storage, NOW));
  assert.equal(
    claimResumeIntent(storage, NOW),
    null,
    'the intent is consumed by the first recording to start'
  );
  assert.equal(
    takeActiveResumeMeetingId(storage),
    null,
    'and the second claim cleared the active id'
  );
}

// --- Re-stashing overwrites rather than accumulating ----------------------------

{
  const storage = fakeStorage();
  stashResumeIntent(storage, 'meeting-one', 'First', NOW);
  stashResumeIntent(storage, 'meeting-two', 'Second', NOW);

  assert.deepEqual(plain(claimResumeIntent(storage, NOW)), {
    meetingId: 'meeting-two',
    title: 'Second',
  });
}

// --- A missing title falls back rather than throwing ----------------------------

{
  const storage = fakeStorage();
  stashResumeIntent(storage, 'meeting-abc', '', NOW);

  assert.deepEqual(plain(claimResumeIntent(storage, NOW)), {
    meetingId: 'meeting-abc',
    title: '',
  });
}

// --- Corrupt expiry is treated as stale, not as forever -------------------------

{
  const storage = fakeStorage({
    pending_resume_meeting_id: 'meeting-abc',
    pending_resume_meeting_title: 'Weekly Sync',
    pending_resume_expires_at: 'not-a-number',
  });

  assert.equal(
    claimResumeIntent(storage, NOW),
    null,
    'an unparseable expiry must fail closed'
  );
}

console.log('resume-intent: all assertions passed');
