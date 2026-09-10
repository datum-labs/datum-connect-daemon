import React, { useState } from 'react';
import { useParams } from 'react-router-dom';
import { useQuery, useMutation, useQueryClient } from 'react-query';
import { toast } from 'react-toastify';
import { api } from '../services/api';
import { useAuth } from '../contexts/AuthContext';

export const PropertyDetail: React.FC = () => {
  const { id } = useParams<{ id: string }>();
  const { user } = useAuth();
  const queryClient = useQueryClient();
  const [selectedSlot, setSelectedSlot] = useState<string>('');
  const [notes, setNotes] = useState('');

  const { data: property, isLoading: propertyLoading } = useQuery(
    ['property', id],
    async () => {
      const response = await api.get(`/properties/${id}`);
      return response.data;
    }
  );

  const { data: slots, isLoading: slotsLoading } = useQuery(
    ['viewingSlots', id],
    async () => {
      const response = await api.get(`/bookings/slots/property/${id}`);
      return response.data;
    }
  );

  const { data: verificationStatus } = useQuery(
    'verificationStatus',
    async () => {
      if (!user || user.role !== 'buyer') return null;
      const response = await api.get('/documents/verification-status');
      return response.data;
    },
    { enabled: !!user && user.role === 'buyer' }
  );

  const bookingMutation = useMutation(
    async (data: { viewingSlotId: string; notes?: string }) => {
      const response = await api.post('/bookings/book', data);
      return response.data;
    },
    {
      onSuccess: () => {
        toast.success('Viewing booked!');
        queryClient.invalidateQueries(['viewingSlots', id]);
        setSelectedSlot('');
        setNotes('');
      },
      onError: (error: any) => {
        toast.error(error.response?.data?.error || 'Booking failed');
      },
    }
  );

  const handleBooking = () => {
    if (!selectedSlot) {
      toast.error('Please select a viewing slot');
      return;
    }
    bookingMutation.mutate({ viewingSlotId: selectedSlot, notes });
  };

  if (propertyLoading) {
    return (
      <div className="flex items-center justify-center py-24 text-dark-utility-3 text-sm">
        <svg className="animate-spin w-4 h-4 mr-2 text-midnight-fjord" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24">
          <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
          <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v8H4z" />
        </svg>
        Loading property…
      </div>
    );
  }

  if (!property) {
    return (
      <div className="banner-danger text-sm">Property not found.</div>
    );
  }

  const canBook = user?.role === 'buyer' && verificationStatus?.canBookViewings;

  return (
    <div className="space-y-6 max-w-3xl">

      {/* Property info card */}
      <div className="card-datum overflow-visible">
        <div className="h-1.5 bg-midnight-fjord w-full rounded-t-datum-lg" />
        <div className="p-6 md:p-8 space-y-6">
          <div>
            <h1 className="text-2xl font-semibold text-midnight-fjord tracking-tight mb-1">
              {property.title}
            </h1>
            <p className="text-sm text-dark-utility-3">{property.address}</p>
          </div>

          <p className="text-3xl font-semibold text-midnight-fjord">
            ${property.price?.toLocaleString()}
          </p>

          {/* Stats */}
          <div className="grid grid-cols-3 gap-3">
            {[
              { label: 'Bedrooms', value: property.bedrooms ?? 'N/A' },
              { label: 'Bathrooms', value: property.bathrooms ?? 'N/A' },
              { label: 'Sq ft', value: property.square_feet?.toLocaleString() ?? 'N/A' },
            ].map(({ label, value }) => (
              <div key={label} className="stat-tile text-center">
                <p className="stat-tile-label">{label}</p>
                <p className="text-xl font-semibold text-midnight-fjord">{value}</p>
              </div>
            ))}
          </div>

          {property.description && (
            <div>
              <h3 className="text-sm font-semibold text-midnight-fjord mb-2">Description</h3>
              <p className="text-sm text-dark-utility-3 leading-relaxed">{property.description}</p>
            </div>
          )}

          {property.property_type && (
            <div className="flex items-center gap-2 text-xs text-dark-utility-3">
              <span className="font-medium text-midnight-fjord">Type:</span>
              <span className="capitalize">{property.property_type}</span>
            </div>
          )}
        </div>
      </div>

      {/* Viewing slots card */}
      <div className="card-datum p-6 md:p-8 space-y-5">
        <h2 className="text-lg font-semibold text-midnight-fjord tracking-tight">
          Available Viewing Slots
        </h2>

        {!user && (
          <div className="banner-warn text-sm">
            Please <a href="/login" className="font-medium underline">sign in</a> to book a viewing slot.
          </div>
        )}

        {user?.role === 'buyer' && !verificationStatus?.canBookViewings && (
          <div className="banner-danger text-sm">
            You need verified financial documents to book viewings.{' '}
            <a href="/documents" className="font-medium underline">Upload documents</a>
          </div>
        )}

        {slotsLoading ? (
          <p className="text-sm text-dark-utility-3">Loading slots…</p>
        ) : slots?.length > 0 ? (
          <div className="space-y-3">
            {slots.map((slot: any) => {
              const start = new Date(slot.start_time);
              const end = new Date(slot.end_time);
              const durationMins = Math.round((end.getTime() - start.getTime()) / 60000);
              const isSelected = selectedSlot === slot.id;

              return (
                <div
                  key={slot.id}
                  className={`flex items-center justify-between rounded-datum-md border p-4 transition-colors duration-150 ${
                    isSelected
                      ? 'border-midnight-fjord bg-glacier-mist-800'
                      : 'border-glacier-mist-900 bg-white hover:bg-glacier-mist-800'
                  }`}
                >
                  <div className="space-y-0.5">
                    <p className="text-sm font-medium text-midnight-fjord">
                      {start.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' })}
                      {' at '}
                      {start.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}
                    </p>
                    <p className="text-xs text-dark-utility-3">
                      {durationMins} min · {slot.available_spots} spot{slot.available_spots !== 1 ? 's' : ''} available
                    </p>
                  </div>

                  {canBook && slot.available_spots > 0 && (
                    <button
                      onClick={() => setSelectedSlot(isSelected ? '' : slot.id)}
                      className={`text-xs font-medium px-4 py-1.5 rounded-datum-md border transition-colors duration-150 ${
                        isSelected
                          ? 'bg-midnight-fjord text-aurora-moss border-midnight-fjord'
                          : 'border-silver-mist text-midnight-fjord hover:bg-glacier-mist-900'
                      }`}
                    >
                      {isSelected ? 'Selected' : 'Select'}
                    </button>
                  )}
                </div>
              );
            })}

            {canBook && selectedSlot && (
              <div className="border-t border-glacier-mist-900 pt-5 space-y-3">
                <label className="field-label">Notes (optional)</label>
                <textarea
                  value={notes}
                  onChange={e => setNotes(e.target.value)}
                  className="field-input resize-none"
                  rows={3}
                  placeholder="Any special requests or questions…"
                />
                <button
                  onClick={handleBooking}
                  disabled={bookingMutation.isLoading}
                  className="btn-datum-primary text-sm px-6 py-2.5 disabled:opacity-50"
                >
                  {bookingMutation.isLoading ? 'Booking…' : 'Confirm booking'}
                </button>
              </div>
            )}
          </div>
        ) : (
          <p className="text-sm text-dark-utility-3">No viewing slots available for this property.</p>
        )}
      </div>
    </div>
  );
};
