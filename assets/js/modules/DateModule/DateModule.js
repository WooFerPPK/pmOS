export default class DateModule {
    formatDate(date) {
        const months = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
        
        const day = date.getDate();
        let daySuffix;

        if (day >= 11 && day <= 13) {
            daySuffix = 'th';
        } else {
            switch (day % 10) {
                case 1:
                    daySuffix = 'st';
                    break;
                case 2:
                    daySuffix = 'nd';
                    break;
                case 3:
                    daySuffix = 'rd';
                    break;
                default:
                    daySuffix = 'th';
            }
        }

        return `${months[date.getMonth()]} ${day}${daySuffix} ${date.getFullYear()}`;
    }

    getDayOfWeek(date) {
        const daysOfWeek = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
        return daysOfWeek[date.getDay()];
    }

    formatTime12Hour(date) {
        let hours = date.getHours();
        const minutes = date.getMinutes();
        const ampm = hours >= 12 ? 'PM' : 'AM';
        
        hours = hours % 12;
        hours = hours ? hours : 12;
        
        return `${hours}:${minutes < 10 ? '0' + minutes : minutes} ${ampm}`;
    }
}