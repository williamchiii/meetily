'use client';

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ImagePlus, Loader2, X, ChevronDown, ChevronUp } from 'lucide-react';
import { useScreenshots } from '@/contexts/ScreenshotContext';
import { useConfig } from '@/contexts/ConfigContext';

/** Providers whose model cannot read an image, regardless of which model is picked. */
const VISION_UNSUPPORTED_PROVIDERS = new Set(['builtin-ai']);

/**
 * Add screenshots to a recording in progress.
 *
 * Slides, dashboards and diagrams carry information nobody reads aloud, so the
 * transcript alone misses it. Each screenshot is read by the selected summary model
 * immediately, and the extracted text is folded into the summary prompt when the
 * meeting is summarised.
 */
export function ScreenshotContextPanel({ isRecording }: { isRecording: boolean }) {
  const { screenshots, isExtracting, addScreenshot, removeScreenshot, setNote } = useScreenshots();
  const { modelConfig } = useConfig();
  const visionUnsupported = VISION_UNSUPPORTED_PROVIDERS.has(modelConfig.provider);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [isDragging, setIsDragging] = useState(false);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const handleFiles = useCallback(
    async (files: FileList | File[] | null) => {
      if (!files || visionUnsupported) return;
      // Sequentially: each one is a model call, and parallel uploads would race the
      // extracting spinner and hammer rate limits
      for (const file of Array.from(files)) {
        await addScreenshot(file);
      }
    },
    [addScreenshot, visionUnsupported]
  );

  // Paste straight from the clipboard - the way people actually take screenshots
  useEffect(() => {
    if (!isRecording || visionUnsupported) return;

    const onPaste = (event: ClipboardEvent) => {
      const target = event.target as HTMLElement | null;
      // Do not steal a paste meant for a text field
      if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) {
        return;
      }

      const images = Array.from(event.clipboardData?.items ?? [])
        .filter(item => item.kind === 'file' && item.type.startsWith('image/'))
        .map(item => item.getAsFile())
        .filter((f): f is File => f !== null);

      if (images.length > 0) {
        event.preventDefault();
        handleFiles(images);
      }
    };

    window.addEventListener('paste', onPaste);
    return () => window.removeEventListener('paste', onPaste);
  }, [isRecording, visionUnsupported, handleFiles]);

  if (!isRecording) return null;

  return (
    <div className="w-full max-w-2xl mx-auto mt-4">
      <div
        onDragOver={e => {
          e.preventDefault();
          setIsDragging(true);
        }}
        onDragLeave={() => setIsDragging(false)}
        onDrop={e => {
          e.preventDefault();
          setIsDragging(false);
          handleFiles(e.dataTransfer.files);
        }}
        className={`border-2 border-dashed rounded-lg p-4 transition-colors ${
          isDragging ? 'border-blue-400 bg-blue-50' : 'border-gray-200 bg-gray-50'
        }`}
      >
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <div className="font-medium text-sm text-gray-800">Screen context</div>
            <div className="text-xs text-gray-600">
              {visionUnsupported
                ? 'Switch to a vision-capable model (Claude, GPT-4o, Gemini, or an Ollama vision model) in Settings to read screenshots.'
                : 'Paste, drop or add a screenshot. The AI reads it so your summary knows what was shown.'}
            </div>
          </div>

          <button
            onClick={() => fileInputRef.current?.click()}
            disabled={isExtracting || visionUnsupported}
            title={visionUnsupported ? 'The current model cannot read images' : undefined}
            className={`flex items-center gap-1.5 px-3 py-1.5 rounded-md text-sm border flex-shrink-0 transition-colors ${
              isExtracting || visionUnsupported
                ? 'border-gray-200 text-gray-400 cursor-not-allowed'
                : 'border-gray-300 text-gray-700 hover:border-blue-400 hover:bg-blue-50'
            }`}
          >
            {isExtracting ? <Loader2 className="w-4 h-4 animate-spin" /> : <ImagePlus className="w-4 h-4" />}
            {isExtracting ? 'Reading…' : 'Add'}
          </button>

          <input
            ref={fileInputRef}
            type="file"
            accept="image/png,image/jpeg,image/webp,image/gif"
            multiple
            disabled={visionUnsupported}
            className="hidden"
            onChange={e => {
              handleFiles(e.target.files);
              e.target.value = '';
            }}
          />
        </div>

        {screenshots.length > 0 && (
          <ul className="mt-3 space-y-2">
            {screenshots.map(shot => (
              <li key={shot.id} className="bg-surface border border-gray-200 rounded-md p-2">
                <div className="flex items-start gap-2">
                  {/* eslint-disable-next-line @next/next/no-img-element */}
                  <img
                    src={shot.previewUrl}
                    alt={shot.fileName}
                    className="w-14 h-14 object-cover rounded border border-gray-200 flex-shrink-0"
                  />

                  <div className="flex-1 min-w-0">
                    <input
                      value={shot.note}
                      onChange={e => setNote(shot.id, e.target.value)}
                      placeholder="Label this screen (optional)"
                      className="w-full text-sm bg-transparent border-b border-transparent hover:border-gray-200 focus:border-blue-400 focus:outline-none py-0.5"
                    />
                    <button
                      onClick={() => setExpandedId(expandedId === shot.id ? null : shot.id)}
                      className="mt-1 flex items-center gap-1 text-xs text-gray-500 hover:text-gray-700"
                    >
                      {expandedId === shot.id ? <ChevronUp className="w-3 h-3" /> : <ChevronDown className="w-3 h-3" />}
                      {shot.extractedText.length} chars read by {shot.model}
                    </button>
                  </div>

                  <button
                    onClick={() => removeScreenshot(shot.id)}
                    className="text-gray-400 hover:text-red-600 flex-shrink-0"
                    aria-label={`Remove ${shot.fileName}`}
                  >
                    <X className="w-4 h-4" />
                  </button>
                </div>

                {expandedId === shot.id && (
                  <pre className="mt-2 text-xs text-gray-700 bg-gray-50 rounded p-2 max-h-40 overflow-auto whitespace-pre-wrap">
                    {shot.extractedText}
                  </pre>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
