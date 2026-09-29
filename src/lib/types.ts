export interface Note {
  id: string;
  title: string | null;
  content: string;
  color: string;
  isPinned: boolean;
  isArchived: boolean;
  deletedAt: string | null;
  window: [number, number, number, number] | null;
  isFloating: boolean;
  createdAt: string;
  modifiedAt: string;
}

export interface ListNotesFilter {
  includeArchived?: boolean;
  deleted?: boolean;
  search?: string;
}
