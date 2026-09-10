import React, { useState } from 'react';
import { Link, useLocation, useNavigate } from 'react-router-dom';
import { useAuth } from '../contexts/AuthContext';

export const Navbar: React.FC = () => {
  const { user, logout } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [mobileOpen, setMobileOpen] = useState(false);

  const handleLogout = () => {
    logout();
    setMobileOpen(false);
    navigate('/');
  };

  const isActive = (path: string) =>
    location.pathname === path || location.pathname.startsWith(path + '/');

  const navLinkClass = (path: string) =>
    `text-sm font-medium px-3 py-1.5 rounded-datum-md transition-colors duration-150 ${
      isActive(path)
        ? 'bg-glacier-mist-800 text-canyon-clay-links font-semibold'
        : 'text-midnight-fjord opacity-80 hover:bg-glacier-mist-800 hover:text-canyon-clay-links hover:opacity-100'
    }`;

  return (
    <nav className="bg-white border-b border-silver-mist w-full">
      <div className="max-w-datum-nav mx-auto px-4 sm:px-7">
        <div className="flex items-center justify-between h-16">

          {/* Logo */}
          <Link
            to="/"
            className="flex items-center gap-2 text-midnight-fjord font-semibold text-base tracking-tight shrink-0"
            onClick={() => setMobileOpen(false)}
          >
            {/* Simple geometric mark */}
            <span className="inline-flex items-center justify-center w-7 h-7 rounded-datum-md bg-midnight-fjord">
              <span className="text-aurora-moss text-xs font-bold">P</span>
            </span>
            PropertyBook
          </Link>

          {/* Desktop nav links */}
          <div className="hidden md:flex items-center gap-1">
            <Link to="/properties" className={navLinkClass('/properties')}>
              Properties
            </Link>
            {user && (
              <Link to="/dashboard" className={navLinkClass('/dashboard')}>
                Dashboard
              </Link>
            )}
            {user?.role === 'buyer' && (
              <Link to="/documents" className={navLinkClass('/documents')}>
                Documents
              </Link>
            )}
          </div>

          {/* Desktop actions */}
          <div className="hidden md:flex items-center h-16 shrink-0">
            {user ? (
              <>
                <span className="text-sm text-dark-utility-3 px-4 border-l border-silver-mist h-full flex items-center">
                  {user.firstName} {user.lastName}
                </span>
                <button
                  onClick={handleLogout}
                  className="h-full px-7 text-sm font-medium text-midnight-fjord border-l border-silver-mist hover:bg-glacier-mist-800 transition-colors duration-150"
                >
                  Log out
                </button>
              </>
            ) : (
              <>
                <Link
                  to="/login"
                  className="h-full px-7 flex items-center text-sm font-medium text-midnight-fjord border-l border-silver-mist hover:bg-glacier-mist-800 transition-colors duration-150"
                >
                  Sign in
                </Link>
                <Link
                  to="/register"
                  className="h-full px-7 flex items-center text-sm font-semibold bg-midnight-fjord text-glacier-mist-700 hover:bg-midnight-fjord/90 transition-colors duration-150"
                >
                  Get started
                </Link>
              </>
            )}
          </div>

          {/* Mobile hamburger */}
          <button
            className="md:hidden flex items-center justify-center w-9 h-9 rounded-datum-md text-midnight-fjord hover:bg-glacier-mist-800 transition-colors"
            onClick={() => setMobileOpen(!mobileOpen)}
            aria-label="Toggle menu"
          >
            {mobileOpen ? (
              <svg xmlns="http://www.w3.org/2000/svg" className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                <path strokeLinecap="round" strokeLinejoin="round" d="M6 18L18 6M6 6l12 12" />
              </svg>
            ) : (
              <svg xmlns="http://www.w3.org/2000/svg" className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                <path strokeLinecap="round" strokeLinejoin="round" d="M4 6h16M4 12h16M4 18h16" />
              </svg>
            )}
          </button>
        </div>
      </div>

      {/* Mobile menu */}
      {mobileOpen && (
        <div className="md:hidden border-t border-silver-mist bg-white">
          <div className="px-4 py-4 flex flex-col gap-1">
            <Link
              to="/properties"
              className={navLinkClass('/properties')}
              onClick={() => setMobileOpen(false)}
            >
              Properties
            </Link>
            {user && (
              <Link
                to="/dashboard"
                className={navLinkClass('/dashboard')}
                onClick={() => setMobileOpen(false)}
              >
                Dashboard
              </Link>
            )}
            {user?.role === 'buyer' && (
              <Link
                to="/documents"
                className={navLinkClass('/documents')}
                onClick={() => setMobileOpen(false)}
              >
                Documents
              </Link>
            )}

            <div className="mt-3 pt-3 border-t border-silver-mist flex flex-col gap-2">
              {user ? (
                <>
                  <span className="text-xs text-dark-utility-3 px-3">
                    {user.firstName} {user.lastName}
                  </span>
                  <button
                    onClick={handleLogout}
                    className="btn-datum-outline text-sm w-full"
                  >
                    Log out
                  </button>
                </>
              ) : (
                <>
                  <Link
                    to="/login"
                    className="btn-datum-outline text-sm text-center"
                    onClick={() => setMobileOpen(false)}
                  >
                    Sign in
                  </Link>
                  <Link
                    to="/register"
                    className="btn-datum-primary text-sm text-center"
                    onClick={() => setMobileOpen(false)}
                  >
                    Get started
                  </Link>
                </>
              )}
            </div>
          </div>
        </div>
      )}
    </nav>
  );
};
