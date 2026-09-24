import '@/styles.css';
import { App } from '@/App';
import { Toaster } from '@datum-cloud/datum-ui/toast';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
    <Toaster position="bottom-right" theme="dark" />
  </StrictMode>,
);
