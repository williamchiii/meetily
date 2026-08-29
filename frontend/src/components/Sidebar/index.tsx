'use client';

import React, { useState, useMemo, useEffect, useCallback, useRef } from 'react';
import { File, Settings, Home, Trash2, Mic, Square, Pencil, NotebookPen, SearchIcon, X, Upload, Folder as FolderIcon, FolderPlus, FolderInput, PanelLeft, MessageCircle, ChevronRight, ChevronDown, CornerUpLeft, MoreHorizontal } from 'lucide-react';
import { useRouter, usePathname, useSearchParams } from 'next/navigation';
import { useSidebar, SIDEBAR_COLLAPSED_WIDTH } from './SidebarProvider';
import type { Folder } from './SidebarProvider';
import { ConfirmationModal } from '../ConfirmationModel/confirmation-modal';
import { ModelConfig } from '@/components/ModelSettingsModal';
import { SettingTabs } from '../SettingTabs';
import { TranscriptModelProps } from '@/components/TranscriptSettings';
import Analytics from '@/lib/analytics';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { useImportDialog } from '@/contexts/ImportDialogContext';
import { useConfig } from '@/contexts/ConfigContext';

import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogTitle,
} from "@/components/ui/dialog"
import { VisuallyHidden } from "@/components/ui/visually-hidden"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { buildFolderParentLabels, buildFolderTree, collectSubtreeIds, flattenFolderTree, folderPath, type FolderNode } from '@/lib/folderTree';

import { MessageToast } from '../MessageToast';
import Info from '../Info';
import { ComplianceNotification } from '../ComplianceNotification';
import { Input } from '../ui/input';
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput } from '../ui/input-group';

const FOLDER_EXPANDED_KEY = 'meetily-expanded-folders';
/** Deeper folders still nest; they just stop gaining left padding. */
const MAX_INDENT_DEPTH = 8;

interface FolderMoveTargetsProps {
  folder: FolderNode;
  folders: Folder[];
  flatFolders: FolderNode[];
  parentLabels: Map<string, string>;
  onMove: (folderId: string, parentId: string | null) => void;
}

/**
 * Destinations for one folder. Radix mounts a submenu only once it opens, so keeping the
 * subtree scan in here means it runs on demand rather than for every row on every render.
 */
const FolderMoveTargets: React.FC<FolderMoveTargetsProps> = ({ folder, folders, flatFolders, parentLabels, onMove }) => {
  // A folder cannot be moved into itself or anything nested below it
  const ownSubtree = collectSubtreeIds(folders, folder.id);
  const targets = flatFolders.filter(candidate => !ownSubtree.has(candidate.id));

  return (
    <>
      <DropdownMenuItem
        disabled={folder.parent_id === null}
        onClick={() => onMove(folder.id, null)}
      >
        <CornerUpLeft className="w-4 h-4 mr-2" />
        Top level
      </DropdownMenuItem>
      {targets.length > 0 && <DropdownMenuSeparator />}
      {targets.map(target => {
        const parentLabel = parentLabels.get(target.id);
        return (
          <DropdownMenuItem
            key={target.id}
            disabled={target.id === folder.parent_id}
            onClick={() => onMove(folder.id, target.id)}
          >
            <FolderIcon className="w-4 h-4 mr-2 flex-shrink-0" />
            <span className="truncate">{target.name}</span>
            {/* Nesting makes duplicate leaf names normal, so name the branch too */}
            {parentLabel && (
              <span className="ml-2 text-xs text-gray-400 truncate">{parentLabel}</span>
            )}
          </DropdownMenuItem>
        );
      })}
      {targets.length === 0 && <DropdownMenuItem disabled>No other folders</DropdownMenuItem>}
    </>
  );
};

const Sidebar: React.FC = () => {
  const router = useRouter();
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const {
    setCurrentMeeting,
    isCollapsed,
    toggleCollapse,
    sidebarWidth,
    setSidebarWidth,
    isResizingSidebar,
    setIsResizingSidebar,
    handleRecordingToggle,
    searchTranscripts,
    searchResults,
    isSearching,
    meetings,
    folders,
    createFolder,
    renameFolder,
    moveFolder,
    deleteFolder,
    serverAddress
  } = useSidebar();

  // Get recording state from RecordingStateContext (single source of truth)
  const { isRecording } = useRecordingState();
  const { openImportDialog } = useImportDialog();
  const { betaFeatures } = useConfig();
  const [searchQuery, setSearchQuery] = useState<string>('');
  const [showModelSettings, setShowModelSettings] = useState(false);
  const [modelConfig, setModelConfig] = useState<ModelConfig>({
    provider: 'ollama',
    model: '',
    whisperModel: '',
    apiKey: null,
    ollamaEndpoint: null
  });
  const [transcriptModelConfig, setTranscriptModelConfig] = useState<TranscriptModelProps>({
    provider: 'parakeet',
    model: 'parakeet-tdt-0.6b-v3-int8',
  });
  const [settingsSaveSuccess, setSettingsSaveSuccess] = useState<boolean | null>(null);

  // Folder nav state (Granola-style sidebar). The active view is derived from the URL
  // rather than mirrored in state, so there is only ever one writer: the router.
  const activeFolderId = pathname === '/notes' ? searchParams.get('folder') : null;
  const isUncategorizedActive =
    pathname === '/notes' && !activeFolderId && searchParams.get('view') === 'uncategorized';
  // Inline "new folder" input; parentId null creates at the top level.
  const [folderDraft, setFolderDraft] = useState<{ parentId: string | null } | null>(null);
  const [newFolderName, setNewFolderName] = useState('');
  // Which folders are expanded in the tree. Everything starts collapsed on a fresh install.
  const [expandedFolderIds, setExpandedFolderIds] = useState<Set<string>>(() => {
    if (typeof window === 'undefined') return new Set();
    try {
      const stored = window.localStorage.getItem(FOLDER_EXPANDED_KEY);
      const parsed = stored ? JSON.parse(stored) : null;
      return new Set(Array.isArray(parsed) ? parsed.filter((id): id is string => typeof id === 'string') : []);
    } catch {
      return new Set();
    }
  });
  const [folderRenameState, setFolderRenameState] = useState<{ isOpen: boolean; folderId: string | null }>({ isOpen: false, folderId: null });
  const [renamingFolderName, setRenamingFolderName] = useState('');
  const [folderDeleteState, setFolderDeleteState] = useState<{ isOpen: boolean; folderId: string | null }>({ isOpen: false, folderId: null });

  // Navigating to a folder should reveal the row it highlights. `undefined` is the first
  // run: launching keeps the tree collapsed by design, so only later moves queue a reveal.
  const lastUrlFolderId = useRef<string | null | undefined>(undefined);
  const pendingExpandId = useRef<string | null>(null);
  useEffect(() => {
    const previous = lastUrlFolderId.current;
    lastUrlFolderId.current = activeFolderId;
    if (previous !== undefined && activeFolderId && activeFolderId !== previous) {
      pendingExpandId.current = activeFolderId;
    }
  }, [activeFolderId]);

  // Also keyed on `folders`, so a navigation that lands before the list loads (or before a
  // freshly created folder is refetched) still opens its branch once the folder is known.
  useEffect(() => {
    const folderId = pendingExpandId.current;
    if (!folderId || !folders.some(folder => folder.id === folderId)) return;
    pendingExpandId.current = null;
    expandTo(folderId);
  }, [folders, activeFolderId]);

  // "New subfolder" mounts an autofocused input while the menu is closing; Radix would
  // hand focus back to the trigger and the input's onBlur would discard the draft.
  const suppressMenuRefocus = useRef(false);

  // useEffect(() => {
  //   if (settingsSaveSuccess !== null) {
  //     const timer = setTimeout(() => {
  //       setSettingsSaveSuccess(null);
  //     }, 3000);
  //   }
  // }, [settingsSaveSuccess]);



  useEffect(() => {
    // Note: Don't set hardcoded defaults - let DB be the source of truth
    const fetchModelConfig = async () => {
      // Only make API call if serverAddress is loaded
      if (!serverAddress) {
        console.log('Waiting for server address to load before fetching model config');
        return;
      }

      try {
        const data = await invoke('api_get_model_config') as any;
        if (data && data.provider !== null) {
          // Fetch API key if not included and provider requires it
          if (data.provider !== 'ollama' && !data.apiKey) {
            try {
              const apiKeyData = await invoke('api_get_api_key', {
                provider: data.provider
              }) as string;
              data.apiKey = apiKeyData;
            } catch (err) {
              console.error('Failed to fetch API key:', err);
            }
          }
          setModelConfig(data);
        }
      } catch (error) {
        console.error('Failed to fetch model config:', error);
      }
    };

    fetchModelConfig();
  }, [serverAddress]);


  useEffect(() => {
    // Note: Don't set hardcoded defaults - let DB be the source of truth
    const fetchTranscriptSettings = async () => {
      // Only make API call if serverAddress is loaded
      if (!serverAddress) {
        console.log('Waiting for server address to load before fetching transcript settings');
        return;
      }

      try {
        const data = await invoke('api_get_transcript_config') as any;
        if (data && data.provider !== null) {
          setTranscriptModelConfig(data);
        }
      } catch (error) {
        console.error('Failed to fetch transcript settings:', error);
      }
    };
    fetchTranscriptSettings();
  }, [serverAddress]);

  // Listen for model config updates from other components
  useEffect(() => {
    const setupListener = async () => {
      const { listen } = await import('@tauri-apps/api/event');
      const unlisten = await listen<ModelConfig>('model-config-updated', (event) => {
        console.log('Sidebar received model-config-updated event:', event.payload);
        setModelConfig(event.payload);
      });

      return unlisten;
    };

    let cleanup: (() => void) | undefined;
    setupListener().then(fn => cleanup = fn);

    return () => {
      cleanup?.();
    };
  }, []);



  // Handle model config save
  const handleSaveModelConfig = async (config: ModelConfig) => {
    try {
      await invoke('api_save_model_config', {
        provider: config.provider,
        model: config.model,
        whisperModel: config.whisperModel,
        apiKey: config.apiKey,
        ollamaEndpoint: config.ollamaEndpoint,
      });

      setModelConfig(config);
      console.log('Model config saved successfully');
      setSettingsSaveSuccess(true);

      // Emit event to sync other components
      const { emit } = await import('@tauri-apps/api/event');
      await emit('model-config-updated', config);

      // Track settings change
      await Analytics.trackSettingsChanged('model_config', `${config.provider}_${config.model}`);
    } catch (error) {
      console.error('Error saving model config:', error);
      setSettingsSaveSuccess(false);
    }
  };

  const handleSaveTranscriptConfig = async (updatedConfig?: TranscriptModelProps) => {
    try {
      const configToSave = updatedConfig || transcriptModelConfig;
      const payload = {
        provider: configToSave.provider,
        model: configToSave.model,
        apiKey: configToSave.apiKey ?? null
      };
      console.log('Saving transcript config with payload:', payload);

      await invoke('api_save_transcript_config', {
        provider: payload.provider,
        model: payload.model,
        apiKey: payload.apiKey,
      });


      setSettingsSaveSuccess(true);

      // Track settings change
      const transcriptConfigToSave = updatedConfig || transcriptModelConfig;
      await Analytics.trackSettingsChanged('transcript_config', `${transcriptConfigToSave.provider}_${transcriptConfigToSave.model}`);
    } catch (error) {
      console.error('Failed to save transcript config:', error);
      setSettingsSaveSuccess(false);
    }
  };

  // Handle search input changes
  const handleSearchChange = useCallback(async (value: string) => {
    setSearchQuery(value);

    // If search query is empty, just return to normal view
    if (!value.trim()) return;

    // Search through transcripts
    await searchTranscripts(value);
  }, [searchTranscripts]);

  // Meetings matching the search, by transcript hit or title, newest first
  const searchMatches = useMemo(() => {
    if (!searchQuery.trim()) return [];
    const q = searchQuery.toLowerCase();
    const transcriptMatches = new Map(searchResults.map(result => [result.id, result]));

    return meetings
      .filter(m => transcriptMatches.has(m.id) || m.title.toLowerCase().includes(q))
      .map(m => ({ ...m, match: transcriptMatches.get(m.id) }))
      .sort((a, b) => new Date(b.created_at ?? 0).getTime() - new Date(a.created_at ?? 0).getTime());
  }, [searchQuery, searchResults, meetings]);

  // Nested folder tree, sorted by name at every level.
  const folderTree = useMemo(() => buildFolderTree(folders), [folders]);
  const flatFolders = useMemo(() => flattenFolderTree(folderTree), [folderTree]);
  const folderParentLabels = useMemo(() => buildFolderParentLabels(folders), [folders]);

  // Drop ids of deleted folders, otherwise the stored list grows with every folder ever
  // expanded. Skipped while the list is empty so the first render cannot wipe it.
  useEffect(() => {
    if (folders.length === 0) return;
    setExpandedFolderIds(prev => {
      const live = new Set(folders.map(folder => folder.id));
      const kept = [...prev].filter(id => live.has(id));
      return kept.length === prev.size ? prev : new Set(kept);
    });
  }, [folders]);

  // Persist expansion so the tree looks the same on the next launch.
  useEffect(() => {
    try {
      window.localStorage.setItem(FOLDER_EXPANDED_KEY, JSON.stringify([...expandedFolderIds]));
    } catch {
      // Persistence is best-effort
    }
  }, [expandedFolderIds]);

  // Meetings without a folder stay easy to find without being mixed into the folder list.
  const uncategorizedMeetings = useMemo(() => {
    return meetings
      .filter(meeting => meeting.folder_id === null || meeting.folder_id === undefined)
      .sort((a, b) => new Date(b.created_at ?? 0).getTime() - new Date(a.created_at ?? 0).getTime());
  }, [meetings]);


  // ----- Folder navigation & CRUD -----

  const openAllNotes = () => router.push('/notes');

  const openUncategorized = () => router.push('/notes?view=uncategorized');

  const openFolder = (folderId: string) => router.push(`/notes?folder=${folderId}`);

  /**
   * Open the branch leading to a folder so its row is on screen. `includeSelf` also opens
   * the folder itself, for when something is about to appear inside it.
   */
  const expandTo = (folderId: string, includeSelf = false) => {
    const path = folderPath(folders, folderId);
    const ids = (includeSelf ? path : path.slice(0, -1)).map(folder => folder.id);
    if (ids.length === 0) return;
    setExpandedFolderIds(prev => {
      const next = new Set(prev);
      ids.forEach(id => next.add(id));
      return next.size === prev.size ? prev : next;
    });
  };

  const toggleFolderExpanded = (folderId: string) => {
    setExpandedFolderIds(prev => {
      const next = new Set(prev);
      if (next.has(folderId)) next.delete(folderId);
      else next.add(folderId);
      return next;
    });
  };

  /** Open the inline name input, either at the top level (null) or inside a folder. */
  const startFolderDraft = (parentId: string | null) => {
    setNewFolderName('');
    setFolderDraft({ parentId });
    // Otherwise the new row would be typed into a collapsed branch
    if (parentId) expandTo(parentId, true);
  };

  const handleCreateFolder = async () => {
    const name = newFolderName.trim();
    const parentId = folderDraft?.parentId ?? null;
    setFolderDraft(null);
    setNewFolderName('');
    if (!name) return;

    const folder = await createFolder(name, parentId);
    if (folder) {
      Analytics.trackButtonClick(parentId ? 'create_subfolder' : 'create_folder', 'sidebar');
      openFolder(folder.id);
    } else {
      toast.error('Failed to create folder');
    }
  };

  const handleMoveFolder = async (folderId: string, parentId: string | null) => {
    const result = await moveFolder(folderId, parentId);
    if (!result.ok) {
      toast.error('Failed to move folder', { description: result.error });
      return;
    }

    if (parentId) {
      // Without opening the whole chain the folder lands somewhere collapsed and looks lost
      expandTo(parentId, true);
      toast.success(`Moved into ${folders.find(f => f.id === parentId)?.name ?? 'folder'}`);
    } else {
      toast.success('Moved to top level');
    }
  };

  // Deleting is blocked while subfolders remain, so say that before opening the modal.
  const requestFolderDelete = (node: FolderNode) => {
    if (node.children.length > 0) {
      toast.error('Folder is not empty', {
        description: `Move or delete its ${node.children.length} subfolder${node.children.length === 1 ? '' : 's'} first.`,
      });
      return;
    }
    setFolderDeleteState({ isOpen: true, folderId: node.id });
  };

  const handleFolderRenameConfirm = async () => {
    const name = renamingFolderName.trim();
    const folderId = folderRenameState.folderId;
    if (!folderId) return;

    if (!name) {
      toast.error('Folder name cannot be empty');
      return;
    }

    const ok = await renameFolder(folderId, name);
    if (ok) {
      toast.success('Folder renamed');
    } else {
      toast.error('Failed to rename folder');
    }
    setFolderRenameState({ isOpen: false, folderId: null });
    setRenamingFolderName('');
  };

  const handleFolderDeleteConfirm = async () => {
    const folderId = folderDeleteState.folderId;
    setFolderDeleteState({ isOpen: false, folderId: null });
    if (!folderId) return;

    const result = await deleteFolder(folderId);
    if (result.ok) {
      toast.success('Folder deleted', { description: 'Its meetings were moved to All Notes' });
      if (activeFolderId === folderId) {
        openAllNotes();
      }
    } else {
      toast.error('Failed to delete folder', { description: result.error });
    }
  };

  // Indent one step per nesting level, but stop stepping past MAX_INDENT_DEPTH: nesting is
  // unlimited while the sidebar is 200-420px wide, and a runaway indent would push a row's
  // name, count and actions menu off the edge where they cannot be reached at all.
  const folderRowPadding = (depth: number) => 12 + Math.min(depth, MAX_INDENT_DEPTH) * 14;

  const renderFolderDraft = (depth: number) => (
    <div
      style={{ paddingLeft: folderRowPadding(depth) }}
      className="pr-2 py-1.5 rounded-md text-sm flex items-center bg-gray-100"
    >
      <span className="w-[22px] flex-shrink-0" />
      <FolderIcon className="w-4 h-4 mr-2 flex-shrink-0 text-gray-500" />
      <input
        autoFocus
        value={newFolderName}
        onChange={(e) => setNewFolderName(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') handleCreateFolder();
          if (e.key === 'Escape') { setFolderDraft(null); setNewFolderName(''); }
        }}
        onBlur={handleCreateFolder}
        placeholder="Folder name"
        className="flex-1 min-w-0 bg-transparent outline-none text-sm placeholder:text-gray-500"
      />
    </div>
  );

  const renderFolderRow = (node: FolderNode): React.ReactNode => {
    const hasChildren = node.children.length > 0;
    const isExpanded = expandedFolderIds.has(node.id);
    const isActive = activeFolderId === node.id;

    return (
      <div key={node.id}>
        <div
          onClick={() => openFolder(node.id)}
          style={{ paddingLeft: folderRowPadding(node.depth) }}
          className={`pr-2 py-1.5 rounded-md text-sm flex items-center group cursor-pointer ${isActive ? 'bg-blue-100 text-blue-700 font-medium' : 'text-gray-700 hover:bg-gray-100'}`}
        >
          {hasChildren ? (
            <button
              onClick={(e) => { e.stopPropagation(); toggleFolderExpanded(node.id); }}
              className="w-5 h-5 mr-0.5 flex items-center justify-center rounded hover:bg-gray-200 flex-shrink-0"
              aria-label={isExpanded ? `Collapse ${node.name}` : `Expand ${node.name}`}
              aria-expanded={isExpanded}
            >
              {isExpanded
                ? <ChevronDown className="w-3.5 h-3.5" />
                : <ChevronRight className="w-3.5 h-3.5" />}
            </button>
          ) : (
            <span className="w-[22px] flex-shrink-0" />
          )}
          <FolderIcon className="w-4 h-4 mr-2 flex-shrink-0" />
          <span className="flex-1 min-w-0 truncate">{node.name}</span>
          {/* group-has keeps the count hidden while the menu is open and the pointer has
              moved off the row into the portalled menu, which would otherwise show both */}
          <span className="ml-2 text-xs text-gray-400 group-hover:hidden group-has-[[data-state=open]]:hidden">
            {node.meeting_count}
          </span>

          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                onClick={(e) => e.stopPropagation()}
                className="hidden group-hover:flex data-[state=open]:flex items-center p-1 rounded-md hover:bg-gray-200 flex-shrink-0"
                aria-label={`Actions for ${node.name}`}
              >
                <MoreHorizontal className="w-3.5 h-3.5" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent
              align="end"
              onClick={(e) => e.stopPropagation()}
              onCloseAutoFocus={(e) => {
                if (!suppressMenuRefocus.current) return;
                suppressMenuRefocus.current = false;
                e.preventDefault();
              }}
            >
              <DropdownMenuItem
                onClick={() => {
                  suppressMenuRefocus.current = true;
                  startFolderDraft(node.id);
                }}
              >
                <FolderPlus className="w-4 h-4 mr-2" />
                New subfolder
              </DropdownMenuItem>
              <DropdownMenuItem
                onClick={() => {
                  setFolderRenameState({ isOpen: true, folderId: node.id });
                  setRenamingFolderName(node.name);
                }}
              >
                <Pencil className="w-4 h-4 mr-2" />
                Rename
              </DropdownMenuItem>
              <DropdownMenuSub>
                <DropdownMenuSubTrigger>
                  <FolderInput className="w-4 h-4 mr-2" />
                  Move to
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent className="max-h-72 overflow-y-auto">
                  <FolderMoveTargets
                    folder={node}
                    folders={folders}
                    flatFolders={flatFolders}
                    parentLabels={folderParentLabels}
                    onMove={handleMoveFolder}
                  />
                </DropdownMenuSubContent>
              </DropdownMenuSub>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-red-500 focus:text-red-500"
                onClick={() => requestFolderDelete(node)}
              >
                <Trash2 className="w-4 h-4 mr-2" />
                Delete
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        {isExpanded && node.children.map(renderFolderRow)}
        {folderDraft?.parentId === node.id && renderFolderDraft(node.depth + 1)}
      </div>
    );
  };

  // Expose setShowModelSettings to window for Rust tray to call
  useEffect(() => {
    (window as any).openSettings = () => {
      setShowModelSettings(true);
    };

    // Cleanup on unmount
    return () => {
      delete (window as any).openSettings;
    };
  }, []);




  // Drag-to-resize from the sidebar's right edge (expanded mode only)
  const startSidebarResize = (e: React.MouseEvent) => {
    e.preventDefault();
    setIsResizingSidebar(true);

    const onMove = (ev: MouseEvent) => setSidebarWidth(ev.clientX);
    const onUp = () => {
      setIsResizingSidebar(false);
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    };

    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
  };

  return (
    <div className="fixed top-0 left-0 h-screen z-40">
      {/* Sidebar toggle - fixed at the window's top-left, Granola-style.
          Same position whether the sidebar is open or fully hidden. */}
      <button
        onClick={toggleCollapse}
        className="fixed top-2 left-2 z-50 p-1.5 rounded-lg text-gray-400 hover:text-gray-600 hover:bg-gray-100 transition-colors"
        aria-label={isCollapsed ? 'Show sidebar' : 'Hide sidebar'}
        title={isCollapsed ? 'Show sidebar' : 'Hide sidebar'}
      >
        <PanelLeft className="w-[18px] h-[18px]" />
      </button>

      <div
        className={`h-screen bg-sidebar shadow-sm flex flex-col relative overflow-hidden border-r ${isResizingSidebar ? '' : 'transition-all duration-300'}`}
        style={{ width: isCollapsed ? SIDEBAR_COLLAPSED_WIDTH : sidebarWidth }}
      >
        {/* Resize handle */}
        {!isCollapsed && (
          <div
            onMouseDown={startSidebarResize}
            className="absolute top-0 right-0 h-full w-1.5 cursor-col-resize z-50 hover:bg-blue-500/40 active:bg-blue-500/60 transition-colors"
            aria-hidden="true"
          />
        )}

        {/* Clearance for the fixed toggle button */}
        <div className="flex-shrink-0 h-11" />

        {/* Collapsed icon rail - quick actions stay reachable without expanding */}
        {isCollapsed && (
          <div className="flex flex-col items-center gap-1 px-1.5">
            <button
              onClick={() => router.push('/')}
              className="w-9 h-9 flex items-center justify-center rounded-lg text-gray-500 hover:text-gray-700 hover:bg-gray-100 transition-colors"
              aria-label="Home"
              title="Home"
            >
              <Home className="w-[18px] h-[18px]" />
            </button>
            <button
              onClick={handleRecordingToggle}
              disabled={isRecording}
              className={`w-9 h-9 flex items-center justify-center rounded-lg text-white transition-colors ${isRecording ? 'bg-red-300 cursor-not-allowed' : 'bg-red-500 hover:bg-red-600'}`}
              aria-label={isRecording ? 'Recording in progress' : 'Start Recording'}
              title={isRecording ? 'Recording in progress' : 'Start Recording'}
            >
              {isRecording ? <Square className="w-4 h-4" /> : <Mic className="w-4 h-4" />}
            </button>
            {betaFeatures.importAndRetranscribe && (
              <button
                onClick={() => openImportDialog()}
                className="w-9 h-9 flex items-center justify-center rounded-lg text-gray-700 bg-blue-100 hover:bg-blue-200 transition-colors"
                aria-label="Import Audio"
                title="Import Audio"
              >
                <Upload className="w-4 h-4" />
              </button>
            )}
            <button
              onClick={() => router.push('/settings')}
              className="w-9 h-9 flex items-center justify-center rounded-lg text-gray-700 bg-gray-200 hover:bg-gray-300 transition-colors"
              aria-label="Settings"
              title="Settings"
            >
              <Settings className="w-4 h-4" />
            </button>
            <Info isCollapsed={isCollapsed} />
          </div>
        )}

        <div className="flex-shrink-0">
          <div className="flex-1">
            {!isCollapsed && (
              <div className="px-3 pb-1">
                <div className="relative mb-1">
                  <InputGroup >
                    <InputGroupInput placeholder='Search meeting content...' value={searchQuery}
                      onChange={(e) => handleSearchChange(e.target.value)}
                    />
                    <InputGroupAddon>
                      <SearchIcon />
                    </InputGroupAddon>
                    {searchQuery &&
                      <InputGroupAddon align={'inline-end'}>
                        <InputGroupButton
                          onClick={() => handleSearchChange('')}
                        >
                          <X />
                        </InputGroupButton>
                      </InputGroupAddon>
                    }
                  </InputGroup>
                </div>
              </div>
            )}
          </div>
        </div>

        {/* Main content - scrollable area */}
        <div className="flex-1 flex flex-col min-h-0">
          {/* Fixed navigation items */}
          <div className="flex-shrink-0">
            {!isCollapsed && (
              <div
                onClick={() => router.push('/')}
                className="px-3 text-sm font-medium text-gray-700 items-center hover:bg-gray-100 h-8 flex mx-3 mt-2 rounded-lg cursor-pointer"
              >
                <Home className="w-4 h-4 mr-2" />
                <span>Home</span>
              </div>
            )}
          </div>

          {/* Content area */}
          <div className="flex-1 flex flex-col min-h-0">
            {/* All Notes + Folders navigation (Granola-style), or search results */}
            {!isCollapsed && (
              <div className="flex-1 overflow-y-auto custom-scrollbar min-h-0 pb-2">
                {searchQuery.trim() ? (
                  <div className="mx-3 mt-3">
                    <div className="px-3 pb-1 text-xs font-semibold uppercase tracking-wider text-gray-400 flex items-center">
                      Results
                      {isSearching && <span className="ml-2 text-blue-500 animate-pulse normal-case font-normal">Searching...</span>}
                    </div>
                    {searchMatches.length === 0 && !isSearching && (
                      <div className="px-3 py-2 text-sm text-gray-500">No matches</div>
                    )}
                    {searchMatches.map(meeting => (
                      <div
                        key={meeting.id}
                        onClick={() => {
                          setCurrentMeeting({ id: meeting.id, title: meeting.title });
                          router.push(`/meeting-details?id=${meeting.id}`);
                        }}
                        className="px-3 py-2 my-0.5 rounded-md text-sm hover:bg-gray-100 cursor-pointer"
                      >
                        <div className="flex items-center">
                          <div className="flex-shrink-0 flex items-center justify-center w-6 h-6 rounded-full mr-2 bg-gray-100">
                            <File className="w-3.5 h-3.5 text-gray-600" />
                          </div>
                          <span className="flex-1 min-w-0 truncate">{meeting.title}</span>
                        </div>
                        {meeting.match && (
                          <div className="mt-1 ml-8 text-xs text-gray-500 bg-yellow-50 p-1.5 rounded border border-yellow-200 line-clamp-2">
                            <span className="font-medium text-yellow-600">Match:</span> {meeting.match.matchContext}
                          </div>
                        )}
                      </div>
                    ))}
                  </div>
                ) : (
                  <>
                    {/* All Notes */}
                    <div
                      onClick={openAllNotes}
                      className={`px-3 text-sm font-medium text-gray-700 items-center h-8 flex mx-3 mt-0.5 rounded-lg cursor-pointer ${pathname === '/notes' && !activeFolderId && !isUncategorizedActive ? 'bg-gray-100' : 'hover:bg-gray-100'}`}
                    >
                      <NotebookPen className="w-4 h-4 mr-2" />
                      <span>All Notes</span>
                    </div>

                    {/* Chat */}
                    <div
                      onClick={() => router.push('/chat')}
                      className={`px-3 text-sm font-medium text-gray-700 items-center h-8 flex mx-3 mt-0.5 rounded-lg cursor-pointer ${pathname === '/chat' ? 'bg-gray-100' : 'hover:bg-gray-100'}`}
                    >
                      <MessageCircle className="w-4 h-4 mr-2" />
                      <span>Chat</span>
                    </div>

                    {/* Virtual folder for meetings that have not been assigned to a folder */}
                    <div className="mx-3 mt-0.5">
                      <div
                        onClick={openUncategorized}
                        className={`px-3 py-1.5 rounded-md text-sm flex items-center group cursor-pointer ${isUncategorizedActive ? 'bg-blue-100 text-blue-700 font-medium' : 'text-gray-700 hover:bg-gray-100'}`}
                      >
                        <FolderIcon className="w-4 h-4 mr-2 flex-shrink-0" />
                        <span className="flex-1 min-w-0 truncate">Uncategorized</span>
                        <span className="ml-2 text-xs text-gray-400">{uncategorizedMeetings.length}</span>
                      </div>
                    </div>

                    {/* Folders */}
                    <div className="mx-3 mt-3 px-3 flex items-center justify-between text-xs font-semibold uppercase tracking-wider text-gray-400">
                      <span>Folders</span>
                      <button
                        onClick={() => startFolderDraft(null)}
                        className="p-1 -mr-1 rounded hover:bg-gray-100 hover:text-gray-600 transition-colors"
                        aria-label="New folder"
                      >
                        <FolderPlus className="w-4 h-4" />
                      </button>
                    </div>

                    <div className="mx-3 mt-0.5">
                      {folderTree.map(renderFolderRow)}

                      {folderDraft?.parentId === null && renderFolderDraft(0)}

                      {folders.length === 0 && !folderDraft && (
                        <div className="px-3 py-2 text-xs text-gray-500">
                          No folders yet — create one to organize your meetings.
                        </div>
                      )}
                    </div>
                  </>
                )}
              </div>
            )}
          </div>
        </div>

        {/* Footer */}
        {!isCollapsed && (

          <div className="flex-shrink-0 p-2 border-t border-gray-100">
            {/* One tight icon row instead of stacked full-width buttons, centred so it
                stays put at any sidebar width */}
            <div className="flex items-center justify-center gap-1">
              <button
                onClick={handleRecordingToggle}
                disabled={isRecording}
                title={isRecording ? 'Recording in progress' : 'Start recording'}
                aria-label={isRecording ? 'Recording in progress' : 'Start recording'}
                className={`w-8 h-8 flex items-center justify-center text-white ${isRecording ? 'bg-red-300 cursor-not-allowed' : 'bg-red-500 hover:bg-red-600'} rounded-lg transition-colors shadow-sm`}
              >
                {isRecording ? <Square className="w-4 h-4" /> : <Mic className="w-4 h-4" />}
              </button>

              {betaFeatures.importAndRetranscribe && (
                <button
                  onClick={() => openImportDialog()}
                  title="Import audio"
                  aria-label="Import audio"
                  className="w-8 h-8 flex items-center justify-center text-gray-700 bg-blue-100 hover:bg-blue-200 rounded-lg transition-colors shadow-sm"
                >
                  <Upload className="w-4 h-4" />
                </button>
              )}

              <button
                onClick={() => router.push('/settings')}
                title="Settings"
                aria-label="Settings"
                className="w-8 h-8 flex items-center justify-center text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors shadow-sm"
              >
                <Settings className="w-4 h-4" />
              </button>

              <Info isCollapsed={isCollapsed} iconOnly />
            </div>
          </div>
        )}
      </div>

      {/* Confirmation Modal for Folder Delete */}
      <ConfirmationModal
        isOpen={folderDeleteState.isOpen}
        text="Delete this folder? Its meetings will not be deleted — they'll move back to All Notes."
        onConfirm={handleFolderDeleteConfirm}
        onCancel={() => setFolderDeleteState({ isOpen: false, folderId: null })}
      />

      {/* Rename Folder Modal */}
      <Dialog open={folderRenameState.isOpen} onOpenChange={(open) => {
        if (!open) {
          setFolderRenameState({ isOpen: false, folderId: null });
          setRenamingFolderName('');
        }
      }}>
        <DialogContent className="sm:max-w-[425px]">
          <VisuallyHidden>
            <DialogTitle>Rename Folder</DialogTitle>
          </VisuallyHidden>
          <div className="py-4">
            <h3 className="text-lg font-semibold mb-4">Rename Folder</h3>
            <div className="space-y-4">
              <div>
                <label htmlFor="folder-name" className="block text-sm font-medium text-gray-700 mb-2">
                  Folder Name
                </label>
                <input
                  id="folder-name"
                  type="text"
                  value={renamingFolderName}
                  onChange={(e) => setRenamingFolderName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      handleFolderRenameConfirm();
                    } else if (e.key === 'Escape') {
                      setFolderRenameState({ isOpen: false, folderId: null });
                      setRenamingFolderName('');
                    }
                  }}
                  className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent"
                  placeholder="Enter folder name"
                  autoFocus
                />
              </div>
            </div>
          </div>
          <DialogFooter>
            <button
              onClick={() => {
                setFolderRenameState({ isOpen: false, folderId: null });
                setRenamingFolderName('');
              }}
              className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-md transition-colors"
            >
              Cancel
            </button>
            <button
              onClick={handleFolderRenameConfirm}
              className="px-4 py-2 text-sm font-medium text-white bg-blue-700 hover:bg-blue-600 rounded-md transition-colors"
            >
              Save
            </button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
};

export default Sidebar;
