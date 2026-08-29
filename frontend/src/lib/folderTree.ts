import type { Folder } from '@/components/Sidebar/SidebarProvider';

export interface FolderNode extends Folder {
  children: FolderNode[];
  /** 0 for a top-level folder, +1 per nesting level - drives sidebar indentation. */
  depth: number;
}

/**
 * Nest a flat folder list by `parent_id`, sorted by name at every level.
 * Folders whose parent is missing surface at the top level rather than disappearing.
 */
export function buildFolderTree(folders: Folder[]): FolderNode[] {
  const byId = new Map<string, FolderNode>(
    folders.map(folder => [folder.id, { ...folder, children: [], depth: 0 }])
  );
  const roots: FolderNode[] = [];

  for (const node of byId.values()) {
    const parent = node.parent_id ? byId.get(node.parent_id) : undefined;
    if (parent && parent.id !== node.id) {
      parent.children.push(node);
    } else {
      roots.push(node);
    }
  }

  // A node unreachable from any root is caught in a parent cycle. The Rust layer
  // rejects those, but lift them out rather than render an endless tree.
  const reachable = new Set<string>();
  const mark = (node: FolderNode) => {
    if (reachable.has(node.id)) return;
    reachable.add(node.id);
    node.children.forEach(mark);
  };
  roots.forEach(mark);

  for (const node of byId.values()) {
    if (reachable.has(node.id)) continue;
    const parent = node.parent_id ? byId.get(node.parent_id) : undefined;
    if (parent) parent.children = parent.children.filter(child => child.id !== node.id);
    roots.push(node);
    mark(node);
  }

  const sortAndMeasure = (nodes: FolderNode[], depth: number) => {
    nodes.sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }));
    for (const node of nodes) {
      node.depth = depth;
      sortAndMeasure(node.children, depth + 1);
    }
  };
  sortAndMeasure(roots, 0);

  return roots;
}

/** Depth-first flatten, each parent immediately before its children. */
export function flattenFolderTree(nodes: FolderNode[]): FolderNode[] {
  const flat: FolderNode[] = [];
  const visit = (node: FolderNode) => {
    flat.push(node);
    node.children.forEach(visit);
  };
  nodes.forEach(visit);
  return flat;
}

/** `folderId` plus every folder nested below it. */
export function collectSubtreeIds(folders: Folder[], folderId: string): Set<string> {
  const childrenByParent = new Map<string, string[]>();
  for (const folder of folders) {
    if (!folder.parent_id) continue;
    const siblings = childrenByParent.get(folder.parent_id);
    if (siblings) siblings.push(folder.id);
    else childrenByParent.set(folder.parent_id, [folder.id]);
  }

  const ids = new Set<string>();
  const queue = [folderId];
  while (queue.length > 0) {
    const id = queue.pop()!;
    if (ids.has(id)) continue;
    ids.add(id);
    queue.push(...(childrenByParent.get(id) ?? []));
  }
  return ids;
}

/** Root-to-folder chain, outermost first. Empty when the folder is unknown. */
export function folderPath(folders: Folder[], folderId: string): Folder[] {
  const byId = new Map(folders.map(folder => [folder.id, folder]));
  const path: Folder[] = [];
  const seen = new Set<string>();

  let current = byId.get(folderId);
  while (current && !seen.has(current.id)) {
    seen.add(current.id);
    path.unshift(current);
    current = current.parent_id ? byId.get(current.parent_id) : undefined;
  }
  return path;
}

/** "Parent / Child" label for menus and breadcrumbs. */
export function folderPathLabel(folders: Folder[], folderId: string, separator = ' / '): string {
  return folderPath(folders, folderId)
    .map(folder => folder.name)
    .join(separator);
}

/**
 * Path label for every folder in one pass. Calling `folderPathLabel` per folder rebuilds
 * the id lookup each time, which is quadratic on lists that refetch after every edit.
 */
export function buildFolderPathLabels(folders: Folder[], separator = ' / '): Map<string, string> {
  const byId = new Map(folders.map(folder => [folder.id, folder]));
  const labels = new Map<string, string>();

  // Ids whose ancestry loops instead of reaching a root
  const cyclic = new Set<string>();
  for (const folder of folders) {
    const seen = new Set<string>();
    let current: Folder | undefined = folder;
    while (current && !seen.has(current.id)) {
      seen.add(current.id);
      current = current.parent_id ? byId.get(current.parent_id) : undefined;
    }
    if (current) seen.forEach(id => cyclic.add(id));
  }

  const labelFor = (id: string, pending: Set<string>): string => {
    const cached = labels.get(id);
    if (cached !== undefined) return cached;

    const folder = byId.get(id);
    if (!folder) return '';
    // Re-entering an id means a parent cycle; stop at the bare name rather than recurse
    if (pending.has(id)) return folder.name;
    pending.add(id);

    const parentLabel = folder.parent_id ? labelFor(folder.parent_id, pending) : '';
    const label = parentLabel ? `${parentLabel}${separator}${folder.name}` : folder.name;
    // A label reached through a cycle is a truncation, not an answer: never cache it,
    // or the first caller's degraded path becomes authoritative for everyone after.
    if (!cyclic.has(id)) labels.set(id, label);
    return label;
  };

  for (const folder of folders) labelFor(folder.id, new Set());
  return labels;
}

/** Ancestor path of each folder, without its own name - "" for a top-level folder. */
export function buildFolderParentLabels(folders: Folder[], separator = ' / '): Map<string, string> {
  const labels = buildFolderPathLabels(folders, separator);
  return new Map(
    folders.map(folder => [folder.id, folder.parent_id ? labels.get(folder.parent_id) ?? '' : ''])
  );
}
