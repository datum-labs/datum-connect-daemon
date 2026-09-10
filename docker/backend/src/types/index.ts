export interface User {
  id: string;
  email: string;
  firstName: string;
  lastName: string;
  phone?: string;
  role: 'buyer' | 'seller' | 'agent' | 'admin';
  isVerified: boolean;
  createdAt: Date;
}

export interface Property {
  id: string;
  title: string;
  description?: string;
  address: string;
  price: number;
  propertyType: string;
  bedrooms?: number;
  bathrooms?: number;
  squareFeet?: number;
  sellerId: string;
  agentId?: string;
  status: 'active' | 'pending' | 'sold' | 'withdrawn';
  createdAt: Date;
}

export interface ViewingSlot {
  id: string;
  propertyId: string;
  startTime: Date;
  endTime: Date;
  isAvailable: boolean;
  maxAttendees: number;
  createdAt: Date;
}

export interface Booking {
  id: string;
  viewingSlotId: string;
  buyerId: string;
  status: 'pending' | 'confirmed' | 'cancelled' | 'completed';
  notes?: string;
  createdAt: Date;
}

export interface FinancialDocument {
  id: string;
  userId: string;
  documentType: string;
  filePath: string;
  verificationStatus: 'pending' | 'verified' | 'rejected';
  verifiedBy?: string;
  verifiedAt?: Date;
  createdAt: Date;
}