import React from 'react';
import { BrowserRouter as Router, Routes, Route } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from 'react-query';
import { ToastContainer } from 'react-toastify';
import 'react-toastify/dist/ReactToastify.css';

import { AuthProvider } from './contexts/AuthContext';
import { ProtectedRoute } from './components/ProtectedRoute';
import { Navbar } from './components/Navbar';
import { Home } from './pages/Home';
import { Login } from './pages/Login';
import { Register } from './pages/Register';
import { Properties } from './pages/Properties';
import { PropertyDetail } from './pages/PropertyDetail';
import { Dashboard } from './pages/Dashboard';
import { DocumentUpload } from './pages/DocumentUpload';

const queryClient = new QueryClient();

function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <Router>
          {/* glacier-mist-900 matches datum's body bg */}
          <div className="min-h-screen bg-glacier-mist-900 flex flex-col">
            <Navbar />
            <main className="flex-1 w-full max-w-datum mx-auto px-4 sm:px-7 py-10">
              <Routes>
                <Route path="/" element={<Home />} />
                <Route path="/login" element={<Login />} />
                <Route path="/register" element={<Register />} />
                <Route path="/properties" element={<Properties />} />
                <Route path="/properties/:id" element={<PropertyDetail />} />
                <Route
                  path="/dashboard"
                  element={
                    <ProtectedRoute>
                      <Dashboard />
                    </ProtectedRoute>
                  }
                />
                <Route
                  path="/documents"
                  element={
                    <ProtectedRoute>
                      <DocumentUpload />
                    </ProtectedRoute>
                  }
                />
              </Routes>
            </main>

            {/* Footer rule */}
            <footer className="border-t border-silver-mist bg-white">
              <div className="max-w-datum mx-auto px-4 sm:px-7 py-6 flex items-center justify-between text-xs text-dark-utility-3">
                <span>© {new Date().getFullYear()} PropertyBook</span>
                <span>Secure property viewings</span>
              </div>
            </footer>
          </div>
          <ToastContainer
            position="top-right"
            toastClassName="!bg-white !text-midnight-fjord !border !border-glacier-mist-900 !shadow-datum-modal !rounded-datum-md !text-sm"
          />
        </Router>
      </AuthProvider>
    </QueryClientProvider>
  );
}

export default App;
