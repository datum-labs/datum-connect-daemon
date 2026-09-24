import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput } from '@datum-cloud/datum-ui/input-group';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@datum-cloud/datum-ui/select';
import { ArrowDownUpIcon, SearchIcon, XIcon } from 'lucide-react';

export interface ToolbarSelect<T extends string> {
  value: T;
  onChange: (value: T) => void;
  options: Record<T, string>;
  label: string;
  icon?: React.ReactNode;
}

export function ListToolbar<S extends string, F extends string = never>({
  query,
  onQueryChange,
  placeholder,
  sort,
  filter,
}: {
  query: string;
  onQueryChange: (query: string) => void;
  placeholder: string;
  sort: ToolbarSelect<S>;
  filter?: ToolbarSelect<F>;
}) {
  return (
    <div className="flex flex-col gap-2 px-2 pb-2">
      <InputGroup className="h-7">
        <InputGroupAddon>
          <SearchIcon />
        </InputGroupAddon>
        <InputGroupInput
          value={query}
          onChange={(e) => onQueryChange(e.target.value)}
          onKeyDown={(e) => e.key === 'Escape' && onQueryChange('')}
          placeholder={placeholder}
          aria-label={placeholder}
          className="h-full border-0 bg-transparent text-xs shadow-none focus-visible:ring-0 focus-visible:ring-offset-0"
        />
        {query && (
          <InputGroupAddon align="inline-end">
            <InputGroupButton size="icon-xs" aria-label="Clear search" onClick={() => onQueryChange('')}>
              <XIcon />
            </InputGroupButton>
          </InputGroupAddon>
        )}
      </InputGroup>
      <div className="flex gap-2">
        {filter && <ToolbarSelectControl {...filter} />}
        <ToolbarSelectControl {...sort} icon={<ArrowDownUpIcon className="size-3.5" />} />
      </div>
    </div>
  );
}

function ToolbarSelectControl<T extends string>({ value, onChange, options, label, icon }: ToolbarSelect<T>) {
  return (
    <Select value={value} onValueChange={(v) => onChange(v as T)}>
      <SelectTrigger aria-label={label} className="h-7 min-h-7 min-w-0 flex-1 gap-1.5 px-2 py-0 text-xs font-normal">
        {icon}
        <span className="min-w-0 flex-1 truncate text-left">
          <SelectValue />
        </span>
      </SelectTrigger>
      <SelectContent>
        {(Object.keys(options) as T[]).map((k) => (
          <SelectItem key={k} value={k} className="text-xs">
            {options[k]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
