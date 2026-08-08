'use client';

import React, { createContext, useCallback, useContext, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';

/**
 * Screenshots shared during a recording.
 *
 * Each one is read by the selected summary model as soon as it is added, so the user
 * can see what was understood while the meeting is still running. They are held here
 * until the recording stops and a meeting exists to attach them to - screenshots are
 * captured before any meeting row is created.
 */

export interface PendingScreenshot {
  /** Client-side id; the database assigns its own on attach. */
  id: string;
  /** Data URI for the thumbnail preview. Not persisted. */
  previewUrl: string;
  fileName: string;
  extractedText: string;
  /** Where the image was written on disk, when a recording was active. */
  imagePath: string | null;
  /** Provider/model that read it. */
  model: string;
  capturedAt: string;
  note: string;
}

interface ScreenshotContextValue {
  screenshots: PendingScreenshot[];
  isExtracting: boolean;
  addScreenshot: (file: File) => Promise<void>;
  removeScreenshot: (id: string) => void;
  setNote: (id: string, note: string) => void;
  clearScreenshots: () => void;
  /** Persist everything gathered so far against a saved meeting. */
  attachToMeeting: (meetingId: string) => Promise<number>;
}

const ScreenshotContext = createContext<ScreenshotContextValue | undefined>(undefined);

export const useScreenshots = () => {
  const context = useContext(ScreenshotContext);
  if (!context) {
    throw new Error('useScreenshots must be used within a ScreenshotProvider');
  }
  return context;
};

/** 20MB is well past what any provider accepts, and past what is useful. */
const MAX_IMAGE_BYTES = 20 * 1024 * 1024;

const ACCEPTED_TYPES = ['image/png', 'image/jpeg', 'image/jpg', 'image/webp', 'image/gif'];

function readAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error('Could not read the image'));
    reader.readAsDataURL(file);
  });
}

export function ScreenshotProvider({ children }: { children: React.ReactNode }) {
  const [screenshots, setScreenshots] = useState<PendingScreenshot[]>([]);
  const [isExtracting, setIsExtracting] = useState(false);

  const addScreenshot = useCallback(async (file: File) => {
    if (!ACCEPTED_TYPES.includes(file.type)) {
      toast.error('That file is not an image', {
        description: 'Add a PNG, JPEG, WebP or GIF screenshot.',
      });
      return;
    }

    if (file.size > MAX_IMAGE_BYTES) {
      toast.error('That screenshot is too large', {
        description: `${(file.size / 1024 / 1024).toFixed(1)}MB exceeds the 20MB limit.`,
      });
      return;
    }

    setIsExtracting(true);

    try {
      const dataUrl = await readAsDataUrl(file);

      const result = await invoke<{
        extracted_text: string;
        image_path: string | null;
        model: string;
      }>('api_extract_screenshot_context', {
        imageBase64: dataUrl,
        mimeType: file.type,
        fileName: file.name,
      });

      const screenshot: PendingScreenshot = {
        id: `pending-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        previewUrl: dataUrl,
        fileName: file.name,
        extractedText: result.extracted_text,
        imagePath: result.image_path,
        model: result.model,
        capturedAt: new Date().toISOString(),
        note: '',
      };

      setScreenshots(prev => [...prev, screenshot]);

      toast.success('Screenshot added to the meeting context', {
        description: `${result.extracted_text.length} characters read by ${result.model}.`,
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      console.error('Screenshot extraction failed:', error);
      toast.error('Could not read that screenshot', { description: message });
    } finally {
      setIsExtracting(false);
    }
  }, []);

  const removeScreenshot = useCallback((id: string) => {
    setScreenshots(prev => prev.filter(s => s.id !== id));
  }, []);

  const setNote = useCallback((id: string, note: string) => {
    setScreenshots(prev => prev.map(s => (s.id === id ? { ...s, note } : s)));
  }, []);

  const clearScreenshots = useCallback(() => setScreenshots([]), []);

  const attachToMeeting = useCallback(async (meetingId: string) => {
    // Read from state at call time rather than closing over it, so a screenshot
    // added moments before the stop is not left behind
    let pending: PendingScreenshot[] = [];
    setScreenshots(current => {
      pending = current;
      return current;
    });

    if (pending.length === 0) {
      return 0;
    }

    const attached = await invoke<number>('api_attach_meeting_screenshots', {
      meetingId,
      screenshots: pending.map(s => ({
        image_path: s.imagePath,
        extracted_text: s.extractedText,
        note: s.note.trim() || null,
        captured_at: s.capturedAt,
      })),
    });

    setScreenshots([]);
    return attached;
  }, []);

  const value = useMemo(
    () => ({
      screenshots,
      isExtracting,
      addScreenshot,
      removeScreenshot,
      setNote,
      clearScreenshots,
      attachToMeeting,
    }),
    [screenshots, isExtracting, addScreenshot, removeScreenshot, setNote, clearScreenshots, attachToMeeting]
  );

  return <ScreenshotContext.Provider value={value}>{children}</ScreenshotContext.Provider>;
}
