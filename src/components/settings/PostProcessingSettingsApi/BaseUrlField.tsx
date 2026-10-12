import React, { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { Input } from "../../ui/Input";

interface BaseUrlFieldProps {
  ariaLabel?: string;
  value: string;
  onBlur: (value: string) => void;
  disabled: boolean;
  disabledReason?: string;
  placeholder?: string;
  className?: string;
}

export const BaseUrlField: React.FC<BaseUrlFieldProps> = React.memo(
  ({
    value,
    onBlur,
    disabled,
    disabledReason,
    placeholder,
    ariaLabel,
    className = "",
  }) => {
    const [localValue, setLocalValue] = useState(value);
    const descriptionId = useId();
    const { t } = useTranslation();

    // Sync with prop changes
    React.useEffect(() => {
      setLocalValue(value);
    }, [value]);

    return (
      <>
        <Input
          aria-label={ariaLabel}
          aria-describedby={disabled ? descriptionId : undefined}
          type="text"
          value={localValue}
          onChange={(event) => setLocalValue(event.target.value)}
          onBlur={() => {
            if (!disabled) onBlur(localValue);
          }}
          placeholder={placeholder}
          variant="compact"
          readOnly={disabled}
          aria-disabled={disabled}
          className={`flex-1 min-w-[360px] ${className}`}
        />
        {disabled && (
          <span id={descriptionId} className="sr-only">
            {disabledReason ??
              t("settings.postProcessing.api.baseUrl.disabledReason")}
          </span>
        )}
      </>
    );
  },
);

BaseUrlField.displayName = "BaseUrlField";
