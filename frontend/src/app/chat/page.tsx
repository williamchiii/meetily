'use client';

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useRouter } from 'next/navigation';
import { invoke } from '@tauri-apps/api/core';
import { ArrowUp, FileText, Loader2, MessageCircle, Trash2 } from 'lucide-react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { toast } from 'sonner';
import { BuiltInAIAPI, BuiltInModelInfo, isModelAvailable } from '@/lib/builtin-ai';

interface OllamaModel {
  name: string;
  id: string;
  size: string;
  modified: string;
}

interface ChatModelConfig {
  provider: string | null;
  model: string | null;
}

/** '' = use the summary model; otherwise 'provider|model' */
function toSelectValue(config: ChatModelConfig): string {
  return config.provider && config.model ? `${config.provider}|${config.model}` : '';
}

interface ChatSource {
  id: string;
  title: string;
  created_at: string;
}

interface ChatResponse {
  answer: string;
  sources: ChatSource[];
}

interface ChatEntry {
  role: 'user' | 'assistant';
  content: string;
  sources?: ChatSource[];
}

const STORAGE_KEY = 'meetily-chat-messages';

function loadStoredMessages(): ChatEntry[] {
  if (typeof window === 'undefined') return [];
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as ChatEntry[]) : [];
  } catch {
    return [];
  }
}

export default function ChatPage() {
  const router = useRouter();
  const [messages, setMessages] = useState<ChatEntry[]>([]);
  const [hydrated, setHydrated] = useState(false);
  const [input, setInput] = useState('');
  const [isLoading, setIsLoading] = useState(false);
  const [modelValue, setModelValue] = useState('');
  const [builtinModels, setBuiltinModels] = useState<BuiltInModelInfo[]>([]);
  const [ollamaModels, setOllamaModels] = useState<OllamaModel[]>([]);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Load the chat model override and the locally available model options
  useEffect(() => {
    invoke<ChatModelConfig>('api_get_chat_model_config')
      .then(config => setModelValue(toSelectValue(config)))
      .catch(error => console.error('Failed to load chat model config:', error));

    BuiltInAIAPI.listModels()
      .then(models => setBuiltinModels(models.filter(m => isModelAvailable(m.status))))
      .catch(() => setBuiltinModels([]));

    invoke<OllamaModel[]>('get_ollama_models', { endpoint: null })
      .then(setOllamaModels)
      .catch(() => setOllamaModels([]));
  }, []);

  const handleModelChange = async (value: string) => {
    const previous = modelValue;
    setModelValue(value);
    const [provider, model] = value ? value.split('|') : [null, null];
    try {
      await invoke('api_save_chat_model_config', { provider, model });
      toast.success(value ? `Chat will use ${model}` : 'Chat will use the summary model');
    } catch (error) {
      setModelValue(previous);
      toast.error('Failed to save chat model', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  // Restore the session's conversation after mount (avoids hydration mismatch
  // with the statically exported empty page)
  useEffect(() => {
    setMessages(loadStoredMessages());
    setHydrated(true);
  }, []);

  useEffect(() => {
    if (!hydrated) return;
    try {
      sessionStorage.setItem(STORAGE_KEY, JSON.stringify(messages));
    } catch {
      // Session storage is best-effort persistence only
    }
  }, [messages, hydrated]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages, isLoading]);

  const sendQuestion = useCallback(async () => {
    const question = input.trim();
    if (!question || isLoading) return;

    const history = messages.map(({ role, content }) => ({ role, content }));
    setMessages(prev => [...prev, { role: 'user', content: question }]);
    setInput('');
    setIsLoading(true);

    try {
      const response = await invoke<ChatResponse>('chat_ask', { question, history });
      setMessages(prev => [
        ...prev,
        { role: 'assistant', content: response.answer, sources: response.sources },
      ]);
    } catch (error) {
      const description = error instanceof Error ? error.message : String(error);
      console.error('Chat request failed:', error);
      toast.error('Chat request failed', { description });
      setMessages(prev => [
        ...prev,
        {
          role: 'assistant',
          content: `Sorry, I couldn't answer that: ${description}`,
        },
      ]);
    } finally {
      setIsLoading(false);
      inputRef.current?.focus();
    }
  }, [input, isLoading, messages]);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendQuestion();
    }
  };

  const clearChat = () => {
    setMessages([]);
    try {
      sessionStorage.removeItem(STORAGE_KEY);
    } catch {
      // best-effort
    }
  };

  const isEmpty = messages.length === 0;

  const modelSelector = (
    <select
      value={modelValue}
      onChange={e => handleModelChange(e.target.value)}
      title="Which model answers chat questions"
      className="max-w-[220px] truncate text-xs text-gray-500 bg-surface border border-gray-200 rounded-md px-2 py-1 cursor-pointer hover:border-gray-300 focus:outline-none focus:border-blue-400 transition-colors"
    >
      <option value="">Model: same as summaries</option>
      {builtinModels.length > 0 && (
        <optgroup label="Built-in AI (downloaded)">
          {builtinModels.map(model => (
            <option key={model.name} value={`builtin-ai|${model.name}`}>
              {model.display_name}
            </option>
          ))}
        </optgroup>
      )}
      {ollamaModels.length > 0 && (
        <optgroup label="Ollama">
          {ollamaModels.map(model => (
            <option key={model.name} value={`ollama|${model.name}`}>
              {model.name}
            </option>
          ))}
        </optgroup>
      )}
    </select>
  );

  const inputBox = (
    <div className="w-full rounded-2xl border border-gray-300 bg-surface shadow-sm focus-within:border-blue-400 transition-colors">
      <textarea
        ref={inputRef}
        value={input}
        onChange={e => setInput(e.target.value)}
        onKeyDown={handleKeyDown}
        placeholder="Ask anything about your meetings…"
        rows={2}
        autoFocus
        className="w-full resize-none bg-transparent px-4 pt-3 pb-1 text-sm text-gray-800 placeholder:text-gray-400 focus:outline-none"
      />
      <div className="flex items-center justify-end px-2 pb-2">
        <button
          onClick={sendQuestion}
          disabled={!input.trim() || isLoading}
          aria-label="Send question"
          className={`p-2 rounded-full transition-colors ${
            input.trim() && !isLoading
              ? 'bg-blue-600 text-white hover:bg-blue-500'
              : 'bg-gray-100 text-gray-400 cursor-not-allowed'
          }`}
        >
          {isLoading ? <Loader2 className="w-4 h-4 animate-spin" /> : <ArrowUp className="w-4 h-4" />}
        </button>
      </div>
    </div>
  );

  return (
    <div className="flex flex-col h-screen bg-surface">
      {isEmpty ? (
        /* Empty state: centered ask-anything, Granola-style */
        <div className="flex-1 flex flex-col items-center justify-center px-8">
          <div className="w-full max-w-2xl -mt-24">
            <h1 className="text-3xl font-semibold text-gray-800 mb-6 text-center">
              Ask anything about your meetings
            </h1>
            {inputBox}
            <div className="mt-4 flex items-center justify-center gap-3">
              {modelSelector}
            </div>
            <p className="mt-3 text-center text-xs text-gray-400">
              Answers are grounded in your local meeting summaries and transcripts.
            </p>
          </div>
        </div>
      ) : (
        <>
          {/* Thread header */}
          <div className="flex-shrink-0 border-b border-gray-200">
            <div className="max-w-2xl mx-auto px-4 py-3 flex items-center justify-between">
              <div className="flex items-center text-sm font-medium text-gray-700">
                <MessageCircle className="w-4 h-4 mr-2" />
                Chat
              </div>
              <div className="flex items-center gap-3">
                {modelSelector}
                <button
                  onClick={clearChat}
                  className="flex items-center gap-1 text-xs text-gray-400 hover:text-red-500 transition-colors"
                  aria-label="Clear conversation"
                >
                  <Trash2 className="w-3.5 h-3.5" />
                  Clear
                </button>
              </div>
            </div>
          </div>

          {/* Messages */}
          <div className="flex-1 overflow-y-auto custom-scrollbar">
            <div className="max-w-2xl mx-auto px-4 py-6 space-y-6">
              {messages.map((message, index) =>
                message.role === 'user' ? (
                  <div key={index} className="flex justify-end">
                    <div className="max-w-[85%] rounded-2xl rounded-br-md bg-blue-100 text-blue-900 px-4 py-2.5 text-sm whitespace-pre-wrap break-words">
                      {message.content}
                    </div>
                  </div>
                ) : (
                  <div key={index} className="text-sm text-gray-800">
                    <div className="chat-markdown break-words">
                      <ReactMarkdown remarkPlugins={[remarkGfm]}>{message.content}</ReactMarkdown>
                    </div>
                    {message.sources && message.sources.length > 0 && (
                      <div className="mt-3 flex flex-wrap gap-1.5">
                        {message.sources.map(source => (
                          <button
                            key={source.id}
                            onClick={() => router.push(`/meeting-details?id=${source.id}`)}
                            className="inline-flex items-center gap-1 px-2.5 py-1 rounded-full text-xs bg-gray-100 text-gray-600 hover:bg-gray-200 hover:text-gray-800 transition-colors"
                            title={`Open "${source.title}"`}
                          >
                            <FileText className="w-3 h-3" />
                            <span className="max-w-[180px] truncate">{source.title}</span>
                          </button>
                        ))}
                      </div>
                    )}
                  </div>
                )
              )}
              {isLoading && (
                <div className="flex items-center gap-2 text-sm text-gray-400">
                  <Loader2 className="w-4 h-4 animate-spin" />
                  Searching transcripts and thinking…
                </div>
              )}
              <div ref={bottomRef} />
            </div>
          </div>

          {/* Input pinned at bottom */}
          <div className="flex-shrink-0 border-t border-gray-200">
            <div className="max-w-2xl mx-auto px-4 py-3">{inputBox}</div>
          </div>
        </>
      )}
    </div>
  );
}
