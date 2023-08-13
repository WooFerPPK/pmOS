export default class DateModule {
    formatDate(date) {
        const months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
        
        const day = date.getDate();
        let daySuffix;

        if (day >= 11 && day <= 13) {
            daySuffix = "th";
        } else {
            switch (day % 10) {
                case 1:
                    daySuffix = "st";
                    break;
                case 2:
                    daySuffix = "nd";
                    break;
                case 3:
                    daySuffix = "rd";
                    break;
                default:
                    daySuffix = "th";
            }
        }

        return `${months[date.getMonth()]} ${day}${daySuffix} ${date.getFullYear()}`;
    }

    getDayOfWeek(date) {
        const daysOfWeek = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
        return daysOfWeek[date.getDay()];
    }

    formatTime12Hour(date) {
        let hours = date.getHours();
        const minutes = date.getMinutes();
        const ampm = hours >= 12 ? 'PM' : 'AM';
        
        hours = hours % 12;
        hours = hours ? hours : 12; // the hour '0' should be '12'
        
        return `${hours}:${minutes < 10 ? '0' + minutes : minutes} ${ampm}`;
    }

    formatTime24Hour(date) {
        const hours = date.getHours();
        const minutes = date.getMinutes();
        
        return `${hours < 10 ? '0' + hours : hours}:${minutes < 10 ? '0' + minutes : minutes}`;
    }

    timeSince(date) {
        const now = new Date();
        const secondsPast = Math.floor((now - date) / 1000);

        if (secondsPast < 60) {
            return `${secondsPast} seconds ago`;
        }
        if (secondsPast < 3600) {
            return `${Math.floor(secondsPast/60)} minutes ago`;
        }
        if (secondsPast <= 86400) {
            return `${Math.floor(secondsPast/3600)} hours ago`;
        }
        if (secondsPast <= 604800) {
            return `${Math.floor(secondsPast/86400)} days ago`;
        }

        return this.formatDate(date);
    }

    convertToTimezone(date, timeZone) {
        try {
            const options = {
                year: 'numeric',
                month: 'long',
                day: 'numeric',
                hour: '2-digit',
                minute: '2-digit',
                second: '2-digit',
                hour12: false,
                timeZone
            };

            return new Intl.DateTimeFormat('en-US', options).format(date);
        } catch (error) {
            return `Error: ${error.message}`;
        }
    }

    durationBetweenDates(startDate, endDate) {
        const durationMillis = endDate - startDate;
        const seconds = Math.floor(durationMillis / 1000);
        const minutes = Math.floor(seconds / 60);
        const hours = Math.floor(minutes / 60);
        const days = Math.floor(hours / 24);

        return {
            days: days,
            hours: hours % 24,
            minutes: minutes % 60,
            seconds: seconds % 60
        };
    }

    durationToString(duration) {
        let result = [];

        if (duration.days) result.push(`${duration.days} days`);
        if (duration.hours) result.push(`${duration.hours} hours`);
        if (duration.minutes) result.push(`${duration.minutes} minutes`);
        if (duration.seconds) result.push(`${duration.seconds} seconds`);

        return result.join(', ');
    }

    addTime(date, {days = 0, hours = 0, minutes = 0, seconds = 0} = {}) {
        const timeToAdd = days * 24 * 60 * 60 * 1000 +
                          hours * 60 * 60 * 1000 +
                          minutes * 60 * 1000 +
                          seconds * 1000;

        return new Date(date.getTime() + timeToAdd);
    }

    subtractTime(date, timeObj) {
        const negativeTimeObj = {
            days: -timeObj.days,
            hours: -timeObj.hours,
            minutes: -timeObj.minutes,
            seconds: -timeObj.seconds
        };

        return this.addTime(date, negativeTimeObj);
    }

    differenceBetweenTimes(startTime, endTime) {
        const difference = endTime - startTime;
        return new Date(difference);
    }
}