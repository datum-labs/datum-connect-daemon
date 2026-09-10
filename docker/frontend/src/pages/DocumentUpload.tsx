import React, { useState } from 'react';
import { useQuery, useMutation, useQueryClient } from 'react-query';
import { toast } from 'react-toastify';
import { api } from '../services/api';

const DOCUMENT_TYPES: { value: string; label: string }[] = [
  { value: 'bank_statement',       label: 'Bank Statement' },
  { value: 'proof_of_income',      label: 'Proof of Income' },
  { value: 'mortgage_preapproval', label: 'Mortgage Pre-approval' },
  { value: 'tax_return',           label: 'Tax Return' },
  { value: 'employment_letter',    label: 'Employment Letter' },
  { value: 'other',                label: 'Other Financial Document' },
];

export const DocumentUpload: React.FC = () => {
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [documentType, setDocumentType] = useState('');
  const queryClient = useQueryClient();

  const { data: documents, isLoading } = useQuery('myDocuments', async () => {
    const response = await api.get('/documents/my-documents');
    return response.data;
  });

  const uploadMutation = useMutation(
    async (formData: FormData) => {
      const response = await api.post('/documents/upload', formData, {
        headers: { 'Content-Type': 'multipart/form-data' },
      });
      return response.data;
    },
    {
      onSuccess: () => {
        toast.success('Document uploaded successfully!');
        queryClient.invalidateQueries('myDocuments');
        setSelectedFile(null);
        setDocumentType('');
      },
      onError: (error: any) => {
        toast.error(error.response?.data?.error || 'Upload failed');
      },
    }
  );

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files?.[0]) setSelectedFile(e.target.files[0]);
  };

  const handleUpload = () => {
    if (!selectedFile || !documentType) {
      toast.error('Please select a file and document type');
      return;
    }
    const formData = new FormData();
    formData.append('document', selectedFile);
    formData.append('documentType', documentType);
    uploadMutation.mutate(formData);
  };

  const statusBadge = (status: string) => {
    switch (status) {
      case 'verified': return <span className="badge-green capitalize">{status}</span>;
      case 'rejected': return <span className="badge-red capitalize">{status}</span>;
      default:         return <span className="badge-yellow capitalize">{status}</span>;
    }
  };

  return (
    <div className="space-y-6 max-w-3xl">

      {/* Info banner */}
      <div className="banner-info text-sm space-y-1">
        <p className="font-semibold text-pine-forge">Document verification required</p>
        <p>
          To book datacenter property viewings, you need to upload and verify your financial capacity documents.
          Your financial details remain private and are only used to confirm your ability to purchase.
        </p>
      </div>

      {/* Upload card */}
      <div className="card-datum p-6 md:p-8 space-y-5">
        <h2 className="text-lg font-semibold text-midnight-fjord tracking-tight">
          Upload a document
        </h2>

        <div className="space-y-4">
          <div>
            <label className="field-label">Document type</label>
            <select
              value={documentType}
              onChange={e => setDocumentType(e.target.value)}
              className="field-select"
            >
              <option value="">Select document type</option>
              {DOCUMENT_TYPES.map(t => (
                <option key={t.value} value={t.value}>{t.label}</option>
              ))}
            </select>
          </div>

          <div>
            <label className="field-label">File</label>
            <input
              type="file"
              onChange={handleFileChange}
              accept=".pdf,.jpg,.jpeg,.png,.doc,.docx"
              className="field-input file:mr-3 file:py-1 file:px-3 file:rounded file:border-0 file:text-xs file:font-medium file:bg-midnight-fjord file:text-glacier-mist-700 hover:file:bg-midnight-fjord/90 cursor-pointer"
            />
            <p className="text-xs text-dark-utility-4 mt-1.5">
              PDF, JPG, PNG, DOC, DOCX — max 10 MB
            </p>
            {selectedFile && (
              <p className="text-xs text-pine-forge mt-1 font-medium">
                Selected: {selectedFile.name}
              </p>
            )}
          </div>

          <button
            onClick={handleUpload}
            disabled={uploadMutation.isLoading || !selectedFile || !documentType}
            className="btn-datum-primary text-sm px-6 py-2.5 disabled:opacity-40"
          >
            {uploadMutation.isLoading ? 'Uploading…' : 'Upload document'}
          </button>
        </div>
      </div>

      {/* Documents list */}
      <div className="card-datum p-6 md:p-8 space-y-5">
        <h2 className="text-lg font-semibold text-midnight-fjord tracking-tight">
          My documents
        </h2>

        {isLoading ? (
          <p className="text-sm text-dark-utility-3">Loading…</p>
        ) : documents?.length > 0 ? (
          <div className="space-y-3">
            {documents.map((doc: any) => (
              <div
                key={doc.id}
                className="flex items-start justify-between gap-4 rounded-datum-md border border-glacier-mist-900 bg-glacier-mist-800 p-4"
              >
                <div className="space-y-0.5 min-w-0">
                  <p className="text-sm font-medium text-midnight-fjord capitalize">
                    {doc.document_type.replace(/_/g, ' ')}
                  </p>
                  <p className="text-xs text-dark-utility-3">
                    Uploaded {new Date(doc.created_at).toLocaleDateString()}
                  </p>
                  {doc.verified_at && (
                    <p className="text-xs text-dark-utility-3">
                      Verified {new Date(doc.verified_at).toLocaleDateString()}
                    </p>
                  )}
                </div>
                {statusBadge(doc.verification_status)}
              </div>
            ))}
          </div>
        ) : (
          <p className="text-sm text-dark-utility-3">No documents uploaded yet.</p>
        )}
      </div>

      {/* Privacy note */}
      <div className="rounded-datum-md border border-glacier-mist-900 bg-white p-5 space-y-2">
        <p className="text-xs font-semibold text-midnight-fjord uppercase tracking-wide">Privacy &amp; Security</p>
        <ul className="text-xs text-dark-utility-3 space-y-1.5">
          <li className="flex items-start gap-2">
            <span className="text-pine-forge mt-0.5">•</span>
            Your documents are encrypted and stored securely
          </li>
          <li className="flex items-start gap-2">
            <span className="text-pine-forge mt-0.5">•</span>
            Only verification status is shared with sellers, not document contents
          </li>
          <li className="flex items-start gap-2">
            <span className="text-pine-forge mt-0.5">•</span>
            Documents are reviewed by certified financial professionals
          </li>
          <li className="flex items-start gap-2">
            <span className="text-pine-forge mt-0.5">•</span>
            You can delete your documents at any time
          </li>
        </ul>
      </div>
    </div>
  );
};
