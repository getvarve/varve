// The fonts ship with the app (no requests to a font service): only the
// weights the UI uses.
import '@fontsource/ibm-plex-sans/400.css';
import '@fontsource/ibm-plex-sans/500.css';
import '@fontsource/ibm-plex-sans/600.css';
import '@fontsource/ibm-plex-mono/400.css';
import '@fontsource/ibm-plex-mono/500.css';
import '@varve/ui/tokens.css';
import { createVarveApp } from '@varve/ui/createVarveApp';

// Varve: the app with no extensions.
createVarveApp({ extensions: [] }).mount('#app');
