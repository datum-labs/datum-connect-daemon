import React from 'react';
import { Link } from 'react-router-dom';

export const Home: React.FC = () => {
  return (
    <div className="space-y-16">

      {/* Hero */}
      <section className="bg-midnight-fjord rounded-datum-lg px-8 py-16 md:px-16 md:py-20 text-center">
        <p className="text-aurora-moss text-xs font-semibold uppercase tracking-widest mb-4">
          Datacenter Property Viewing Platform
        </p>
        <h1 className="text-3xl md:text-4xl lg:text-5xl font-semibold text-white tracking-tight leading-tight mb-6 max-w-2xl mx-auto">
          Book Datacenter property viewings with confidence
        </h1>
        <p className="text-dark-utility-4 text-base md:text-lg leading-relaxed mb-10 max-w-xl mx-auto">
          Secure viewings with verified financial capacity — protecting both buyers and sellers throughout the process.
        </p>
        <div className="flex flex-col sm:flex-row gap-3 justify-center">
          <Link to="/register" className="btn-datum-accent px-6 py-3 text-sm">
            Get started
          </Link>
          <Link to="/properties" className="btn-datum-outline px-6 py-3 text-sm border-white/20 text-glacier-mist-700 hover:bg-white/10">
            Browse properties
          </Link>
        </div>
      </section>

      {/* Feature cards */}
      <section>
        <h2 className="section-heading text-center mb-2">Who it's for</h2>
        <p className="text-center text-dark-utility-3 text-sm mb-8">Two roles, one platform</p>
        <div className="grid md:grid-cols-3 gap-5">
          <div className="card-datum p-6">
            <div className="w-8 h-8 rounded-datum-md bg-aurora-mist flex items-center justify-center mb-4">
              <svg xmlns="http://www.w3.org/2000/svg" className="w-4 h-4 text-pine-forge" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                <path strokeLinecap="round" strokeLinejoin="round" d="M16 7a4 4 0 11-8 0 4 4 0 018 0zM12 14a7 7 0 00-7 7h14a7 7 0 00-7-7z" />
              </svg>
            </div>
            <h3 className="text-base font-semibold text-midnight-fjord mb-2">For Buyers</h3>
            <p className="text-sm text-dark-utility-3 leading-relaxed mb-5">
              Upload financial documents securely and book datacenter property viewings with verified capacity.
            </p>
            <Link to="/register" className="btn-datum-primary text-xs px-4 py-2">
              Start as Buyer
            </Link>
          </div>

          <div className="card-datum p-6">
            <div className="w-8 h-8 rounded-datum-md bg-aurora-mist flex items-center justify-center mb-4">
              <svg xmlns="http://www.w3.org/2000/svg" className="w-4 h-4 text-pine-forge" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                <path strokeLinecap="round" strokeLinejoin="round" d="M3 12l2-2m0 0l7-7 7 7M5 10v10a1 1 0 001 1h3m10-11l2 2m-2-2v10a1 1 0 01-1 1h-3m-6 0a1 1 0 001-1v-4a1 1 0 011-1h2a1 1 0 011 1v4a1 1 0 001 1m-6 0h6" />
              </svg>
            </div>
            <h3 className="text-base font-semibold text-midnight-fjord mb-2">For Sellers</h3>
            <p className="text-sm text-dark-utility-3 leading-relaxed mb-5">
              List properties and manage viewing slots with confidence in buyer qualifications.
            </p>
            <Link to="/register" className="btn-datum-primary text-xs px-4 py-2">
              Start as Seller
            </Link>
          </div>

          <div className="card-datum p-6">
            <div className="w-8 h-8 rounded-datum-md bg-aurora-mist flex items-center justify-center mb-4">
              <svg xmlns="http://www.w3.org/2000/svg" className="w-4 h-4 text-pine-forge" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                <path strokeLinecap="round" strokeLinejoin="round" d="M12 15v2m-6 4h12a2 2 0 002-2v-6a2 2 0 00-2-2H6a2 2 0 00-2 2v6a2 2 0 002 2zm10-10V7a4 4 0 00-8 0v4h8z" />
              </svg>
            </div>
            <h3 className="text-base font-semibold text-midnight-fjord mb-2">Secure &amp; Private</h3>
            <p className="text-sm text-dark-utility-3 leading-relaxed">
              Financial details remain private while proving capacity to purchase. Only verification status is shared.
            </p>
          </div>
        </div>
      </section>

      {/* How it works */}
      <section className="bg-white rounded-datum-lg border border-glacier-mist-900 px-8 py-10 md:px-12 md:py-12">
        <h2 className="section-heading mb-1">How it works</h2>
        <p className="text-sm text-dark-utility-3 mb-10">Four steps from sign-up to viewing</p>

        <div className="grid sm:grid-cols-2 lg:grid-cols-4 gap-8">
          {[
            { n: '1', title: 'Register', body: 'Create your account as a buyer or seller.' },
            { n: '2', title: 'Verify', body: 'Buyers upload financial documents for secure verification.' },
            { n: '3', title: 'Book a Viewing', body: 'Browse available properties and select a viewing slot.' },
            { n: '4', title: 'Calendar Sync', body: 'Download .ics or add directly to Gmail or Outlook.' },
          ].map(({ n, title, body }) => (
            <div key={n} className="flex flex-col gap-3">
              <div className="w-8 h-8 rounded-full bg-midnight-fjord text-aurora-moss text-xs font-bold flex items-center justify-center shrink-0">
                {n}
              </div>
              <h4 className="text-sm font-semibold text-midnight-fjord">{title}</h4>
              <p className="text-sm text-dark-utility-3 leading-relaxed">{body}</p>
            </div>
          ))}
        </div>
      </section>

    </div>
  );
};
