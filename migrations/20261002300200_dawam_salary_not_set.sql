-- Owner decision D9 (24 Sep 2026): a salary can be "not set". Someone a
-- manager adds or imports (a manager may not set pay) is stored with a NULL
-- salary, never a silent 0: payroll then flags them and refuses approval
-- until the owner sets it or marks them not on payroll.
--
-- Only the NOT NULL goes; the default and every existing row stay as they
-- are (existing 0 salaries are left alone). The salary-history trigger
-- learns that NULL means "no salary yet": it writes no history row for it,
-- and the first real salary counts from the hire date, as a first salary
-- after 0 already does.
ALTER TABLE employees ALTER COLUMN base_salary_piastres DROP NOT NULL;

CREATE OR REPLACE FUNCTION employees_salary_history_trg() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.base_salary_piastres IS NULL THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'INSERT' THEN
        INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
        VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, COALESCE(NEW.hire_date, CURRENT_DATE))
        ON CONFLICT (employee_id, effective_from) DO UPDATE SET base_salary_piastres = EXCLUDED.base_salary_piastres;
    ELSIF NEW.base_salary_piastres IS DISTINCT FROM OLD.base_salary_piastres THEN
        IF COALESCE(OLD.base_salary_piastres, 0) = 0 THEN
            DELETE FROM employee_salary_history WHERE employee_id = NEW.id;
            INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
            VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, COALESCE(NEW.hire_date, CURRENT_DATE));
        ELSE
            INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
            VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, CURRENT_DATE)
            ON CONFLICT (employee_id, effective_from) DO UPDATE SET base_salary_piastres = EXCLUDED.base_salary_piastres;
        END IF;
    END IF;
    RETURN NEW;
END $$;
