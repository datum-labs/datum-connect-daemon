import React from 'react';
import { useQuery } from 'react-query';
import { Link } from 'react-router-dom';
import { api } from '../services/api';

export const Properties: React.FC = () => {
  const { data, isLoading, error } = useQuery('properties', async () => {
    const response = await api.get('/properties');
    return response.data;
  });

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-24 text-dark-utility-3 text-sm">
        <svg className="animate-spin w-4 h-4 mr-2 text-midnight-fjord" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24">
          <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
          <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v8H4z" />
        </svg>
        Loading properties…
      </div>
    );
  }

  if (error) {
    return (
      <div className="banner-danger text-sm">
        Error loading properties. Please try again.
      </div>
    );
  }

  return (
    <div className="space-y-8">
      <div>
        <h1 className="text-2xl font-semibold text-midnight-fjord tracking-tight mb-1">
          Available Properties
        </h1>
        <p className="text-sm text-dark-utility-3">
          {data?.properties?.length ?? 0} {data?.properties?.length === 1 ? 'property' : 'properties'} listed
        </p>
      </div>

      {data?.properties?.length === 0 ? (
        <div className="card-datum p-12 text-center text-sm text-dark-utility-3">
          No properties available at the moment. Check back soon.
        </div>
      ) : (
        <div className="grid md:grid-cols-2 lg:grid-cols-3 gap-5">
          {data?.properties?.map((property: any) => (
            <div key={property.id} className="card-datum flex flex-col">
              {/* Colour band */}
              <div className="h-1.5 bg-midnight-fjord w-full" />

              <div className="p-6 flex flex-col flex-1 gap-3">
                <div>
                  <h3 className="text-base font-semibold text-midnight-fjord leading-snug mb-1">
                    {property.title}
                  </h3>
                  <p className="text-xs text-dark-utility-3">{property.address}</p>
                </div>

                <p className="text-xl font-semibold text-midnight-fjord">
                  ${property.price?.toLocaleString()}
                </p>

                <div className="flex gap-4 text-xs text-dark-utility-3 border-t border-glacier-mist-900 pt-3">
                  <span>{property.bedrooms} bed</span>
                  <span>{property.bathrooms} bath</span>
                  <span>{property.square_feet?.toLocaleString()} sq ft</span>
                </div>

                <p className="text-sm text-dark-utility-3 leading-relaxed line-clamp-3 flex-1">
                  {property.description}
                </p>

                <Link
                  to={`/properties/${property.id}`}
                  className="btn-datum-primary text-xs self-start px-4 py-2 mt-1"
                >
                  View details
                </Link>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
};
