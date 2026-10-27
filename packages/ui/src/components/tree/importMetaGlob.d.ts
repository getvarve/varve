/**
 * Vite's build-time `import.meta.glob` (the `vite/client` types are not a
 * dependency of this package). `WorkspaceView` uses it to load the view of
 * each tab kind from the directory of the worker that builds it.
 */
interface ImportMeta {
  glob<T = unknown>(pattern: string | readonly string[], options?: { eager?: false; import?: string }): Record<string, () => Promise<T>>;
}
