import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { createNote, listNotes } from './api';

const invokeMock = vi.mocked(invoke);
beforeEach(() => invokeMock.mockReset());

describe('notes API', () => {
  it('creates a note with no arguments', async () => {
    invokeMock.mockResolvedValue({ id: 'n1' });
    await expect(createNote()).resolves.toMatchObject({ id: 'n1' });
    expect(invokeMock).toHaveBeenCalledWith('create_note');
  });

  it('maps list filters to camel-case command arguments', async () => {
    invokeMock.mockResolvedValue([]);
    await listNotes({ search: 'milk' });
    expect(invokeMock).toHaveBeenCalledWith('list_notes', {
      includeArchived: false, deleted: false, search: 'milk',
    });
  });
});
