import { invoke } from '@tauri-apps/api/core';
import type { ListNotesFilter, Note } from './types';

export const createNote = () => invoke<Note>('create_note');
export const getNote = (id: string) => invoke<Note | null>('get_note', { id });
export const listNotes = ({ includeArchived = false, deleted = false, search }: ListNotesFilter = {}) =>
  invoke<Note[]>('list_notes', { includeArchived, deleted, search: search ?? null });
export const saveNote = (id: string, content: string) => invoke<void>('save_note', { id, content });
export const updateNoteMeta = (id: string, values: { color?: string; isPinned?: boolean; isArchived?: boolean }) =>
  invoke<void>('update_note_meta', { id, color: values.color ?? null, isPinned: values.isPinned ?? null, isArchived: values.isArchived ?? null });
export const softDelete = (id: string) => invoke<void>('soft_delete', { id });
export const restoreNote = (id: string) => invoke<void>('restore_note', { id });
export const purgeNote = (id: string) => invoke<void>('purge_note', { id });
export const saveWindowGeom = (id: string, x: number, y: number, w: number, h: number, floating: boolean) =>
  invoke<void>('save_window_geom', { id, x, y, w, h, floating });
export const openNoteWindow = (id: string) => invoke<void>('open_note_window', { id });
export const openBoard = () => invoke<void>('open_board');
export const closeNoteWindow = (id: string) => invoke<void>('close_note_window', { id });
export const destroyNoteWindow = (id: string) => invoke<void>('destroy_note_window', { id });
export const dragNoteWindow = (id: string) => invoke<void>('drag_note_window', { id });
