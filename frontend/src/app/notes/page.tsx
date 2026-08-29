'use client';

import React, { Suspense, useEffect, useMemo, useState } from 'react';
import { useRouter, useSearchParams } from 'next/navigation';
import { format, isToday, isYesterday } from 'date-fns';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { ArrowLeft, ChevronRight, File, Folder as FolderIcon, FolderMinus, MoreHorizontal, NotebookPen, Pencil, Trash2 } from 'lucide-react';

import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import type { CurrentMeeting } from '@/components/Sidebar/SidebarProvider';
import { ConfirmationModal } from '@/components/ConfirmationModel/confirmation-modal';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogTitle,
} from '@/components/ui/dialog';
import { VisuallyHidden } from '@/components/ui/visually-hidden';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Switch } from '@/components/ui/switch';
import Analytics from '@/lib/analytics';
import { buildFolderParentLabels, buildFolderPathLabels, buildFolderTree, collectSubtreeIds, flattenFolderTree, folderPath } from '@/lib/folderTree';

// Whether a folder view also lists meetings from its subfolders, remembered for the session.
const INCLUDE_SUBFOLDERS_KEY = 'meetily-notes-include-subfolders';

// Group label in the style of Granola's My Notes list
function groupLabel(date: Date): string {
  if (isToday(date)) return 'Today';
  if (isYesterday(date)) return 'Yesterday';
  if (date.getFullYear() === new Date().getFullYear()) return format(date, 'EEE, MMM d');
  return format(date, 'MMM d, yyyy');
}

interface MeetingGroup {
  key: string;
  label: string;
  meetings: CurrentMeeting[];
}

function NotesContent() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const folderId = searchParams.get('folder');
  const isUncategorized = searchParams.get('view') === 'uncategorized';

  const {
    meetings,
    folders,
    currentMeeting,
    setCurrentMeeting,
    moveMeetingToFolder,
    refetchMeetings,
    refetchFolders,
  } = useSidebar();

  const [renameState, setRenameState] = useState<{ isOpen: boolean; meetingId: string | null }>({ isOpen: false, meetingId: null });
  const [renameTitle, setRenameTitle] = useState('');
  const [deleteState, setDeleteState] = useState<{ isOpen: boolean; meetingId: string | null }>({ isOpen: false, meetingId: null });

  const [includeSubfolders, setIncludeSubfolders] = useState<boolean>(() => {
    if (typeof window === 'undefined') return false;
    try {
      return window.sessionStorage.getItem(INCLUDE_SUBFOLDERS_KEY) === 'true';
    } catch {
      return false;
    }
  });

  useEffect(() => {
    try {
      window.sessionStorage.setItem(INCLUDE_SUBFOLDERS_KEY, String(includeSubfolders));
    } catch {
      // Persistence is best-effort
    }
  }, [includeSubfolders]);

  const activeFolder = !isUncategorized && folderId ? folders.find(f => f.id === folderId) ?? null : null;
  // Full "Parent / Child" labels so a nested folder is never ambiguous in the list or menus
  const folderPathById = useMemo(() => buildFolderPathLabels(folders), [folders]);
  const folderParentLabels = useMemo(() => buildFolderParentLabels(folders), [folders]);
  const folderMenuItems = useMemo(() => flattenFolderTree(buildFolderTree(folders)), [folders]);
  const breadcrumb = useMemo(
    () => (activeFolder ? folderPath(folders, activeFolder.id).slice(0, -1) : []),
    [activeFolder, folders]
  );
  // Subfolders of the folder being viewed, listed above the meetings as their own rows
  const childFolders = useMemo(() => {
    if (!activeFolder) return [];
    return folders
      .filter(f => f.parent_id === activeFolder.id)
      .sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }));
  }, [activeFolder, folders]);
  const hasSubfolders = childFolders.length > 0;

  // One level up: the enclosing folder, or All Notes from a top-level folder
  const parentFolder = activeFolder?.parent_id
    ? folders.find(f => f.id === activeFolder.parent_id) ?? null
    : null;
  const goUp = () => router.push(parentFolder ? `/notes?folder=${parentFolder.id}` : '/notes');

  // Direct counts, matching the sidebar badge and the list you land on when you click
  // through. The subfolder tally is what says a branch folder still holds something.
  const meetingsByFolder = useMemo(() => {
    const counts = new Map<string, number>();
    for (const meeting of meetings) {
      if (!meeting.folder_id) continue;
      counts.set(meeting.folder_id, (counts.get(meeting.folder_id) ?? 0) + 1);
    }
    return counts;
  }, [meetings]);

  const subfoldersByFolder = useMemo(() => {
    const counts = new Map<string, number>();
    for (const folder of folders) {
      if (!folder.parent_id) continue;
      counts.set(folder.parent_id, (counts.get(folder.parent_id) ?? 0) + 1);
    }
    return counts;
  }, [folders]);

  // Folder ids the list covers: the folder alone, or its whole subtree when the toggle is on
  const scopeIds = useMemo(() => {
    if (!activeFolder) return null;
    return includeSubfolders
      ? collectSubtreeIds(folders, activeFolder.id)
      : new Set([activeFolder.id]);
  }, [activeFolder, folders, includeSubfolders]);

  // Meetings in scope, newest first, grouped by calendar day
  const groups = useMemo<MeetingGroup[]>(() => {
    const inScope = scopeIds
      ? meetings.filter(m => m.folder_id != null && scopeIds.has(m.folder_id))
      : isUncategorized
        ? meetings.filter(m => m.folder_id === null || m.folder_id === undefined)
      : meetings;

    const sorted = [...inScope].sort((a, b) =>
      new Date(b.created_at ?? 0).getTime() - new Date(a.created_at ?? 0).getTime()
    );

    const result: MeetingGroup[] = [];
    for (const meeting of sorted) {
      const date = meeting.created_at ? new Date(meeting.created_at) : null;
      const valid = date && !isNaN(date.getTime());
      const key = valid ? format(date!, 'yyyy-MM-dd') : 'undated';
      const label = valid ? groupLabel(date!) : 'Undated';

      const last = result[result.length - 1];
      if (last && last.key === key) {
        last.meetings.push(meeting);
      } else {
        result.push({ key, label, meetings: [meeting] });
      }
    }
    return result;
  }, [meetings, scopeIds, isUncategorized]);

  const meetingCount = groups.reduce((n, g) => n + g.meetings.length, 0);

  const openMeeting = (meeting: CurrentMeeting) => {
    setCurrentMeeting({ id: meeting.id, title: meeting.title });
    router.push(`/meeting-details?id=${meeting.id}`);
  };

  const handleMove = async (meetingId: string, targetFolderId: string | null) => {
    const ok = await moveMeetingToFolder(meetingId, targetFolderId);
    if (ok) {
      const name = targetFolderId ? folderPathById.get(targetFolderId) : null;
      toast.success(name ? `Moved to ${name}` : 'Removed from folder');
    } else {
      toast.error('Failed to move meeting');
    }
  };

  const handleRenameConfirm = async () => {
    const title = renameTitle.trim();
    const meetingId = renameState.meetingId;
    if (!meetingId) return;
    if (!title) {
      toast.error('Meeting title cannot be empty');
      return;
    }

    try {
      await invoke('api_save_meeting_title', { meetingId, title });
      await refetchMeetings();
      toast.success('Meeting title updated');
    } catch (error) {
      toast.error('Failed to update meeting title', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
    setRenameState({ isOpen: false, meetingId: null });
    setRenameTitle('');
  };

  const handleDeleteConfirm = async () => {
    const meetingId = deleteState.meetingId;
    setDeleteState({ isOpen: false, meetingId: null });
    if (!meetingId) return;

    try {
      await invoke('api_delete_meeting', { meetingId });
      Analytics.trackMeetingDeleted(meetingId);
      await refetchMeetings();
      await refetchFolders();
      // Don't leave currentMeeting pointing at a row that no longer exists
      if (currentMeeting?.id === meetingId) {
        setCurrentMeeting({ id: 'intro-call', title: '+ New Call' });
      }
      toast.success('Meeting deleted', { description: 'All associated data has been removed' });
    } catch (error) {
      toast.error('Failed to delete meeting', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  return (
    <div className="flex flex-col h-screen bg-surface">
      <div className="flex-1 overflow-y-auto custom-scrollbar">
        <div className="max-w-3xl mx-auto px-8 py-12">
          {/* Header */}
          <div className="flex items-center gap-3">
            {activeFolder && (
              <button
                onClick={goUp}
                title={parentFolder ? `Back to ${parentFolder.name}` : 'Back to All Notes'}
                aria-label={parentFolder ? `Back to ${parentFolder.name}` : 'Back to All Notes'}
                className="flex-shrink-0 flex items-center justify-center w-8 h-8 -ml-1 rounded-lg text-gray-500 hover:text-gray-900 hover:bg-gray-100 transition-colors"
              >
                <ArrowLeft className="w-5 h-5" />
              </button>
            )}
            <div className="flex items-center justify-center w-10 h-10 rounded-xl bg-gray-100 border border-gray-200">
              {activeFolder || isUncategorized ? (
                <FolderIcon className="w-5 h-5 text-gray-500" />
              ) : (
                <NotebookPen className="w-5 h-5 text-gray-500" />
              )}
            </div>
            <div>
              {/* Where a nested folder sits, with each ancestor clickable */}
              {breadcrumb.length > 0 && (
                <div className="flex items-center flex-wrap text-xs text-gray-500 mb-0.5">
                  {breadcrumb.map((ancestor, index) => (
                    <React.Fragment key={ancestor.id}>
                      {index > 0 && <ChevronRight className="w-3 h-3 mx-0.5 text-gray-400" />}
                      <button
                        onClick={() => router.push(`/notes?folder=${ancestor.id}`)}
                        className="hover:text-gray-800 hover:underline"
                      >
                        {ancestor.name}
                      </button>
                    </React.Fragment>
                  ))}
                </div>
              )}
              <h1 className="text-3xl font-bold text-gray-900">
                {activeFolder ? activeFolder.name : isUncategorized ? 'Uncategorized' : 'All Notes'}
              </h1>
              <p className="text-sm text-gray-500 mt-0.5">
                {activeFolder
                  ? `${meetingCount} ${meetingCount === 1 ? 'meeting' : 'meetings'} ${includeSubfolders && hasSubfolders ? 'in this folder and its subfolders' : 'in this folder'}`
                  : isUncategorized
                    ? `${meetingCount} ${meetingCount === 1 ? 'meeting' : 'meetings'} without a folder`
                  : `Notes from all of your meetings · ${meetingCount} ${meetingCount === 1 ? 'meeting' : 'meetings'}`}
              </p>
            </div>

            {/* Nested folders are separate buckets by default; widen the view on demand */}
            {hasSubfolders && (
              <label className="ml-auto flex items-center gap-2 text-sm text-gray-600 cursor-pointer select-none">
                <span>Include subfolders</span>
                <Switch
                  checked={includeSubfolders}
                  onCheckedChange={setIncludeSubfolders}
                  aria-label="Include meetings from subfolders"
                />
              </label>
            )}
          </div>

          {/* Subfolders, kept visually distinct from notes by the folder icon */}
          {childFolders.length > 0 && (
            <div className="mt-6">
              <div className="text-sm font-medium text-gray-500 mb-1 px-3">Folders</div>
              {childFolders.map(folder => {
                const notes = meetingsByFolder.get(folder.id) ?? 0;
                const nested = subfoldersByFolder.get(folder.id) ?? 0;
                return (
                  <div
                    key={folder.id}
                    onClick={() => router.push(`/notes?folder=${folder.id}`)}
                    className="flex items-center gap-2.5 px-3 py-1.5 rounded-lg hover:bg-gray-100 cursor-pointer group transition-colors"
                  >
                    <div className="flex-shrink-0 flex items-center justify-center w-7 h-7 rounded-md bg-gray-100 border border-gray-200">
                      <FolderIcon className="w-3.5 h-3.5 text-gray-500" />
                    </div>

                    <div className="flex-1 min-w-0">
                      <div className="text-sm font-medium text-gray-900 truncate">{folder.name}</div>
                      <div className="text-xs text-gray-500 mt-0.5">
                        {notes} {notes === 1 ? 'note' : 'notes'}
                        {nested > 0 && ` · ${nested} ${nested === 1 ? 'subfolder' : 'subfolders'}`}
                      </div>
                    </div>

                    <ChevronRight className="w-4 h-4 flex-shrink-0 text-gray-400" />
                  </div>
                );
              })}
            </div>
          )}

          {/* Empty states */}
          {meetingCount === 0 && (
            <div className={childFolders.length > 0 ? 'mt-10 text-center' : 'mt-16 text-center'}>
              <p className="text-gray-500">
                {activeFolder
                  ? 'No meetings in this folder yet.'
                  : isUncategorized
                    ? 'No uncategorized meetings yet.'
                  : 'No meetings yet.'}
              </p>
              <p className="text-sm text-gray-400 mt-1">
                {activeFolder
                  ? hasSubfolders && !includeSubfolders
                    ? 'Its subfolders may hold notes — turn on “Include subfolders” to see them.'
                    : 'Use the ⋯ menu on any note to move it here.'
                  : isUncategorized
                    ? 'Meetings without a folder will appear here.'
                  : 'Start a recording to create your first meeting note.'}
              </p>
            </div>
          )}

          {/* Grouped list */}
          {groups.map(group => (
            <div key={group.key} className="mt-6">
              <div className="text-sm font-medium text-gray-500 mb-1 px-3">{group.label}</div>
              {group.meetings.map(meeting => {
                const date = meeting.created_at ? new Date(meeting.created_at) : null;
                const timeLabel = date && !isNaN(date.getTime()) ? format(date, 'h:mm a') : '';
                // Only worth showing when it tells you something the header does not
                const folderName = meeting.folder_id && meeting.folder_id !== activeFolder?.id
                  ? folderPathById.get(meeting.folder_id)
                  : null;

                return (
                  <div
                    key={meeting.id}
                    onClick={() => openMeeting(meeting)}
                    className="flex items-center gap-2.5 px-3 py-1.5 rounded-lg hover:bg-gray-100 cursor-pointer group transition-colors"
                  >
                    <div className="flex-shrink-0 flex items-center justify-center w-7 h-7 rounded-md bg-gray-100 border border-gray-200">
                      <File className="w-3.5 h-3.5 text-gray-500" />
                    </div>

                    <div className="flex-1 min-w-0">
                      <div className="text-sm font-medium text-gray-900 truncate">{meeting.title}</div>
                      {folderName && (
                        <div className="flex items-center gap-1 text-xs text-gray-500 mt-0.5">
                          <FolderIcon className="w-3 h-3" />
                          <span className="truncate">{folderName}</span>
                        </div>
                      )}
                    </div>

                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <button
                          onClick={(e) => e.stopPropagation()}
                          className="p-1.5 rounded-md text-gray-400 hover:text-gray-700 hover:bg-gray-200 opacity-0 group-hover:opacity-100 data-[state=open]:opacity-100 transition-opacity"
                          aria-label="Meeting actions"
                        >
                          <MoreHorizontal className="w-4 h-4" />
                        </button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
                        <DropdownMenuSub>
                          <DropdownMenuSubTrigger>
                            <FolderIcon className="w-4 h-4 mr-2" />
                            Move to folder
                          </DropdownMenuSubTrigger>
                          <DropdownMenuSubContent className="max-h-80 overflow-y-auto">
                            {folderMenuItems.length === 0 && (
                              <DropdownMenuItem disabled>No folders yet</DropdownMenuItem>
                            )}
                            {folderMenuItems.map(folder => {
                              const parentLabel = folderParentLabels.get(folder.id);
                              return (
                                <DropdownMenuItem
                                  key={folder.id}
                                  disabled={meeting.folder_id === folder.id}
                                  onClick={() => handleMove(meeting.id, folder.id)}
                                >
                                  <FolderIcon className="w-4 h-4 mr-2 flex-shrink-0" />
                                  <span className="truncate">{folder.name}</span>
                                  {/* Nesting makes duplicate leaf names normal, so name the branch too */}
                                  {parentLabel && (
                                    <span className="ml-2 text-xs text-gray-400 truncate">{parentLabel}</span>
                                  )}
                                </DropdownMenuItem>
                              );
                            })}
                            {meeting.folder_id && (
                              <>
                                <DropdownMenuSeparator />
                                <DropdownMenuItem onClick={() => handleMove(meeting.id, null)}>
                                  <FolderMinus className="w-4 h-4 mr-2" />
                                  Remove from folder
                                </DropdownMenuItem>
                              </>
                            )}
                          </DropdownMenuSubContent>
                        </DropdownMenuSub>
                        <DropdownMenuItem
                          onClick={() => {
                            setRenameState({ isOpen: true, meetingId: meeting.id });
                            setRenameTitle(meeting.title);
                          }}
                        >
                          <Pencil className="w-4 h-4 mr-2" />
                          Rename
                        </DropdownMenuItem>
                        <DropdownMenuSeparator />
                        <DropdownMenuItem
                          className="text-red-500 focus:text-red-500"
                          onClick={() => setDeleteState({ isOpen: true, meetingId: meeting.id })}
                        >
                          <Trash2 className="w-4 h-4 mr-2" />
                          Delete
                        </DropdownMenuItem>
                      </DropdownMenuContent>
                    </DropdownMenu>

                    <span className="flex-shrink-0 text-xs text-gray-500 tabular-nums w-16 text-right">
                      {timeLabel}
                    </span>
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      </div>

      {/* Delete confirmation */}
      <ConfirmationModal
        isOpen={deleteState.isOpen}
        text="Are you sure you want to delete this meeting? This action cannot be undone."
        onConfirm={handleDeleteConfirm}
        onCancel={() => setDeleteState({ isOpen: false, meetingId: null })}
      />

      {/* Rename dialog */}
      <Dialog open={renameState.isOpen} onOpenChange={(open) => {
        if (!open) {
          setRenameState({ isOpen: false, meetingId: null });
          setRenameTitle('');
        }
      }}>
        <DialogContent className="sm:max-w-[425px]">
          <VisuallyHidden>
            <DialogTitle>Rename Meeting</DialogTitle>
          </VisuallyHidden>
          <div className="py-4">
            <h3 className="text-lg font-semibold mb-4">Rename Meeting</h3>
            <input
              type="text"
              value={renameTitle}
              onChange={(e) => setRenameTitle(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') handleRenameConfirm();
                if (e.key === 'Escape') {
                  setRenameState({ isOpen: false, meetingId: null });
                  setRenameTitle('');
                }
              }}
              className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              placeholder="Enter meeting title"
              autoFocus
            />
          </div>
          <DialogFooter>
            <button
              onClick={() => {
                setRenameState({ isOpen: false, meetingId: null });
                setRenameTitle('');
              }}
              className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-md transition-colors"
            >
              Cancel
            </button>
            <button
              onClick={handleRenameConfirm}
              className="px-4 py-2 text-sm font-medium text-white bg-blue-700 hover:bg-blue-600 rounded-md transition-colors"
            >
              Save
            </button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

export default function NotesPage() {
  return (
    <Suspense fallback={<div className="flex items-center justify-center h-screen bg-surface" />}>
      <NotesContent />
    </Suspense>
  );
}
