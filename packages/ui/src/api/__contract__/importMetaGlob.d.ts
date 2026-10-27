/**
 * The eager form of Vite's `import.meta.glob` (the lazy form is declared in
 * components/tree/importMetaGlob.d.ts). Tests use it to read sample files
 * and component sources at build time.
 */
interface ImportMeta {
  glob<T = unknown>(pattern: string | readonly string[], options: { eager: true; import?: string; query?: string }): Record<string, T>;
}
