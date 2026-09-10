import React from 'react';
import { useQuery } from 'react-query';
import { api } from '../services/api';
import { useAuth } from '../contexts/AuthContext';

export const Dashboard: React.FC = () => {
  const { user } = useAuth();

  const { data: bookings } = useQuery(
    'myBookings',
    async () => {
      const response = await api.get('/bookings/my-bookings');
      return response.data;
    },
    { enabled: user?.role === 'buyer' }
  );

  const downloadICS = async (bookingId: string) => {
    try {
      const response = await api.get(`/calendar/booking/${bookingId}/ics`, {
        responseType: 'blob',
      });
      const blob = new Blob([response.data], { type: 'text/calendar' });
      const url = window.URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = url;
      link.download = `property-viewing-${bookingId}.ics`;
      link.click();
      window.URL.revokeObjectURL(url);
    } catch {
      console.error('Error downloading ICS file');
    }
  };

  const openGmailCalendar = async (bookingId: string) => {
    try {
      const response = await api.get(`/calendar/booking/${bookingId}/gmail`);
      window.open(response.data.url, '_blank');
    } catch {
      console.error('Error opening Gmail calendar');
    }
  };

  const openOutlookCalendar = async (bookingId: string) => {
    try {
      const response = await api.get(`/calendar/booking/${bookingId}/outlook`);
      window.open(response.data.url, '_blank');
    } catch {
      console.error('Error opening Outlook calendar');
    }
  };

  const statusBadge = (status: string) => {
    switch (status) {
      case 'confirmed': return <span className="badge-green capitalize">{status}</span>;
      case 'pending':   return <span className="badge-yellow capitalize">{status}</span>;
      default:          return <span className="badge-red capitalize">{status}</span>;
    }
  };

  return (
    <div className="space-y-8 max-w-3xl">

      {/* Welcome card */}
      <div className="card-datum p-6 md:p-8">
        <h1 className="text-xl font-semibold text-midnight-fjord tracking-tight mb-5">
          Welcome back, {user?.firstName}
        </h1>
        <div className="grid sm:grid-cols-3 gap-3">
          <div className="stat-tile">
            <p className="stat-tile-label">Role</p>
            <p className="stat-tile-value capitalize">{user?.role}</p>
          </div>
          <div className="stat-tile">
            <p className="stat-tile-label">Email</p>
            <p className="stat-tile-value truncate">{user?.email}</p>
          </div>
          <div className="stat-tile">
            <p className="stat-tile-label">Status</p>
            <p className="stat-tile-value">
              {user?.isVerified ? 'Verified' : 'Pending verification'}
            </p>
          </div>
        </div>
      </div>

      {/* Bookings (buyers only) */}
      {user?.role === 'buyer' && (
        <div className="card-datum p-6 md:p-8 space-y-5">
          <h2 className="text-lg font-semibold text-midnight-fjord tracking-tight">
            My Bookings
          </h2>

          {bookings?.length > 0 ? (
            <div className="space-y-3">
              {bookings.map((booking: any) => (
                <div
                  key={booking.id}
                  className="rounded-datum-md border border-glacier-mist-900 bg-glacier-mist-800 p-4 space-y-3"
                >
                  <div className="flex items-start justify-between gap-4">
                    <div className="space-y-0.5 min-w-0">
                      <h3 className="text-sm font-semibold text-midnight-fjord leading-snug truncate">
                        {booking.property_title}
                      </h3>
                      <p className="text-xs text-dark-utility-3">{booking.property_address}</p>
                      <p className="text-xs text-dark-utility-3">
                        {new Date(booking.start_time).toLocaleDateString(undefined, {
                          weekday: 'short', month: 'short', day: 'numeric',
                        })}
                        {' at '}
                        {new Date(booking.start_time).toLocaleTimeString([], {
                          hour: '2-digit', minute: '2-digit',
                        })}
                      </p>
                    </div>
                    {statusBadge(booking.status)}
                  </div>

                  {booking.notes && (
                    <p className="text-xs text-dark-utility-3 bg-white rounded-datum-sm px-3 py-2 border border-glacier-mist-900">
                      <span className="font-medium text-midnight-fjord">Note: </span>
                      {booking.notes}
                    </p>
                  )}

                  {booking.status === 'confirmed' && (
                    <div className="flex flex-wrap gap-2 pt-1 border-t border-glacier-mist-900">
                      <button
                        onClick={() => downloadICS(booking.id)}
                        className="btn-datum-outline text-xs px-3 py-1.5"
                      >
                        Download .ics
                      </button>
                      <button
                        onClick={() => openGmailCalendar(booking.id)}
                        className="btn-datum-outline text-xs px-3 py-1.5"
                      >
                        Add to Gmail
                      </button>
                      <button
                        onClick={() => openOutlookCalendar(booking.id)}
                        className="btn-datum-outline text-xs px-3 py-1.5"
                      >
                        Add to Outlook
                      </button>
                    </div>
                  )}
                </div>
              ))}
            </div>
          ) : (
            <p className="text-sm text-dark-utility-3">
              No bookings yet.{' '}
              <a href="/properties" className="text-canyon-clay-links hover:underline">
                Browse properties
              </a>{' '}
              to book a datacenter tour.
            </p>
          )}
        </div>
      )}
    </div>
  );
};
