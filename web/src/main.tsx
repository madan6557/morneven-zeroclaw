import React from 'react';
import ReactDOM from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import '@fontsource/inter/latin-400.css';
import '@fontsource/inter/latin-500.css';
import '@fontsource/inter/latin-600.css';
import '@fontsource/inter/latin-700.css';
import '@fontsource/jetbrains-mono/latin-400.css';
import '@fontsource/jetbrains-mono/latin-500.css';
import '@fontsource/jetbrains-mono/latin-600.css';
import App from './App';
import { basePath } from './lib/basePath';
import './index.css';

const unsupportedBrowser =
  document.documentElement.getAttribute('data-unsupported-browser') === '1';

if (!unsupportedBrowser) {
  ReactDOM.createRoot(document.getElementById('root')!).render(
    <React.StrictMode>
      {/* basePath is injected by the Rust gateway at serve time for reverse-proxy prefix support. */}
      <BrowserRouter basename={basePath || '/'}>
        <App />
      </BrowserRouter>
    </React.StrictMode>
  );
}
