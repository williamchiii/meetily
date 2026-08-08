/**
 * Storage Service
 *
 * Handles all meeting storage and retrieval Tauri backend calls (SQLite persistence).
 * Pure 1-to-1 wrapper - no error handling changes, exact same behavior as direct invoke calls.
 */

import { invoke } from '@tauri-apps/api/core';
import { Transcript } from '@/types';

export interface SaveMeetingRequest {
  meetingTitle: string;
  transcripts: Transcript[];
  folderPath: string | null;
}

export interface SaveMeetingResponse {
  meeting_id: string;
}

export interface Meeting {
  id: string;
  title: string;
  [key: string]: any; // Allow additional properties from backend
}

export interface MergeRecordingResponse {
  merged: boolean;
  detail: string;
  duration_seconds?: number | null;
  /** Where the merged-from folder was moved, when it could be retired. */
  retired_to?: string | null;
}

/**
 * Storage Service
 * Singleton service for managing meeting storage operations
 */
export class StorageService {
  /**
   * Save meeting transcript to SQLite database
   * @param meetingTitle - Title of the meeting
   * @param transcripts - Array of transcript segments
   * @param folderPath - Optional folder path for audio file
   * @returns Promise with { meeting_id: string }
   */
  async saveMeeting(
    meetingTitle: string,
    transcripts: Transcript[],
    folderPath: string | null
  ): Promise<SaveMeetingResponse> {
    return invoke<SaveMeetingResponse>('api_save_transcript', {
      meetingTitle,
      transcripts,
      folderPath,
    });
  }

  /**
   * Append transcript segments to a meeting that already exists.
   * Used when a recording is resumed after it stopped, so both stretches of the
   * conversation end up in one meeting instead of two.
   * @param meetingId - ID of the meeting to append to
   * @param transcripts - Transcript segments from the resumed recording
   */
  async appendToMeeting(
    meetingId: string,
    transcripts: Transcript[]
  ): Promise<SaveMeetingResponse> {
    return invoke<SaveMeetingResponse>('api_append_transcript', {
      meetingId,
      transcripts,
    });
  }

  /**
   * Fold a resumed recording's audio and sidecar files into the meeting it continues,
   * so the meeting folder holds the whole conversation rather than just its first half.
   * @param meetingId - Meeting that was appended to
   * @param resumedFolderPath - Folder the resumed recording saved into
   */
  async mergeResumedRecording(
    meetingId: string,
    resumedFolderPath: string
  ): Promise<MergeRecordingResponse> {
    return invoke<MergeRecordingResponse>('api_merge_resumed_recording', {
      meetingId,
      resumedFolderPath,
    });
  }

  /**
   * Get meeting details by ID
   * @param meetingId - ID of the meeting to fetch
   * @returns Promise with meeting details
   */
  async getMeeting(meetingId: string): Promise<Meeting> {
    return invoke<Meeting>('api_get_meeting', { meetingId });
  }

  /**
   * Get list of all meetings
   * @returns Promise with array of meetings
   */
  async getMeetings(): Promise<Meeting[]> {
    return invoke<Meeting[]>('api_get_meetings');
  }
}

// Export singleton instance
export const storageService = new StorageService();
