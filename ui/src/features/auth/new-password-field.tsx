"use client";

import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import {
  PasswordInput,
  PasswordInputControl,
  PasswordInputToggle,
} from "@ui-sdk/react/password-input";
import { useState } from "react";
import { CheckIcon, TodoIcon } from "@/components/icons";

// The server's policy: 12 to 128 characters, counted as Unicode characters. `pattern` rather than
// `minLength`, which a value set by script never trips.
const LENGTH = ".{12,128}";

export function NewPasswordField({
  name,
  label,
  invalid,
}: {
  name: string;
  label: string;
  invalid: boolean;
}) {
  const [value, setValue] = useState("");
  const length = [...value].length;
  const fits = length >= 12 && length <= 128;
  return (
    <Field name={name} invalid={invalid || undefined}>
      <FieldLabel>{label}</FieldLabel>
      <PasswordInput
        required
        autoComplete="new-password"
        value={value}
        onValueChange={(next) => setValue(next)}
      >
        <PasswordInputControl pattern={LENGTH} />
        <PasswordInputToggle aria-label="Show password" />
      </PasswordInput>
      <FieldDescription className="flex items-center gap-1.5">
        {fits ? <CheckIcon className="shrink-0" /> : <TodoIcon className="shrink-0" />}
        <span className="sr-only">{fits ? "Done:" : "To do:"}</span>
        12 to 128 characters
      </FieldDescription>
      <FieldError match="valueMissing">Enter a password.</FieldError>
      <FieldError match="patternMismatch">Use 12 to 128 characters.</FieldError>
      <FieldError match={invalid}>Use 12 to 128 characters.</FieldError>
    </Field>
  );
}
