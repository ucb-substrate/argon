import {createContext, useContext, type ReactNode} from 'react';

export type EditorId = 'neovim' | 'vscode';

const EditorContext = createContext<EditorId | undefined>(undefined);

function useEditor(): EditorId {
  const editor = useContext(EditorContext);
  if (editor === undefined) {
    throw new Error('Editor-specific content must be inside <ForEditor>.');
  }
  return editor;
}

/** Renders shared tutorial content for one editor. */
export function ForEditor({editor, children}: {editor: EditorId; children: ReactNode}) {
  return <EditorContext.Provider value={editor}>{children}</EditorContext.Provider>;
}

/** Content shown only in the Neovim tutorial. */
export function Neovim({children}: {children: ReactNode}) {
  return useEditor() === 'neovim' ? <>{children}</> : null;
}

/** Content shown only in the VS Code tutorial. */
export function VSCode({children}: {children: ReactNode}) {
  return useEditor() === 'vscode' ? <>{children}</> : null;
}

/** The current tutorial's editor name. */
export function EditorName() {
  return <>{useEditor() === 'neovim' ? 'Neovim' : 'VS Code'}</>;
}
