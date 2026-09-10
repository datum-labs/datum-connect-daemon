# Property Viewing Booking System

A web application for the property buying/selling industry that allows potential buyers to book viewing slots while providing financial capacity verification without disclosing sensitive financial details to sellers.

## Features

- **Secure Document Verification**: Buyers upload financial documents for verification without exposing details to sellers
- **Viewing Slot Booking**: Book available property viewing slots
- **Calendar Integration**: Download .ics files and add to Gmail/Outlook calendars
- **CRUD Operations**: Full property, user, and booking management
- **Containerized Deployment**: Docker-based setup for easy deployment

## Tech Stack

- **Frontend**: React with TypeScript
- **Backend**: Node.js with Express and TypeScript
- **Database**: PostgreSQL
- **Authentication**: JWT-based auth
- **File Storage**: Secure document handling
- **Containerization**: Docker & Docker Compose

## Quick Start

```bash
# Clone and start the application
docker-compose up --build
```

The application will be available at:
- Frontend: http://localhost:3000
- Backend API: http://localhost:5000
- Database: PostgreSQL on port 5432